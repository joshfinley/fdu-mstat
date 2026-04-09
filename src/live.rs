//! Live-updating event loop using epoll, timerfd, and signalfd.
//!
//! When `--live` (or `--live=N`) is passed on the command line, the report
//! enters an alternate terminal screen and refreshes on a timer. The loop
//! is purely event-driven: `epoll_wait` blocks indefinitely, consuming zero
//! CPU between updates.
//!
//! ## Data collection tiers
//!
//! Not all data changes at the same rate, so collection is split into tiers
//! to minimize I/O per tick:
//!
//! - **Fast** (every tick): load averages, memory, CPU frequency, uptime.
//!   Total I/O per tick is ~1.7KB across 4 tiny /proc and /sys reads.
//! - **Slow** (every ~30s): disk usage via `statvfs(2)`.
//! - **Static** (startup only): OS, kernel, hostname, IPs, CPU model,
//!   core topology, hypervisor, DNS, login info. Never re-collected.
//!
//! ## Event sources
//!
//! - `timerfd` — periodic fast-data refresh (default 2s, configurable).
//! - `signalfd` — SIGWINCH triggers layout recompute + full redraw;
//!   SIGINT/SIGTERM triggers clean exit with terminal restore.
//!
//! ## Terminal handling
//!
//! On entry: save termios, disable echo/canonical mode, enter alternate
//! screen, hide cursor. On exit (including signals): restore all of it.
//! Rendering uses cursor-home + full overwrite (~3.5KB per tick), which
//! is robust against terminal quirks and trivial at 2s intervals.

use crate::buf::Buf;
use crate::frame::Frame;
use crate::render::{Layout, compute_layout, render};
use crate::sys::{
    DiskData, P_CPU_FREQ, P_UPTIME, SysInfo, collect_disk, collect_mem, find_val, parse_f64_b,
    parse_u64_b, raw_read,
};

// --- Paths used only by the fast collector ----------------------------------

const P_CPUINFO: *const u8 = b"/proc/cpuinfo\0".as_ptr();
const P_LOADAVG: *const u8 = b"/proc/loadavg\0".as_ptr();

// --- epoll event tags -------------------------------------------------------

const EV_TIMER: u64 = 1;
const EV_SIGNAL: u64 = 2;

// --- Argument parsing -------------------------------------------------------

/// Parse `--live` or `--live=N` from argv.
///
/// Returns the refresh interval in seconds, or 0 if live mode was not
/// requested. Default interval is 2 seconds.
pub fn parse_live_interval() -> u64 {
    for arg in std::env::args_os().skip(1) {
        let b = arg.as_encoded_bytes();
        if b == b"--live" {
            return 2;
        }
        if b.starts_with(b"--live=") {
            let n = parse_u64_b(&b[7..]);
            return if n == 0 { 2 } else { n };
        }
    }
    0
}

// --- Fast data collection ---------------------------------------------------

/// Re-collect only fast-changing system data.
///
/// Called every tick. Reads ~1.7KB total from /proc and /sys:
/// - `/proc/loadavg` (~45 bytes) for 1/5/15-minute load averages
/// - sysfs `scaling_cur_freq` (~5 bytes) for current CPU frequency,
///   with `/proc/cpuinfo` as a fallback for systems without cpufreq
/// - `/proc/meminfo` (~1.5KB) for memory usage
/// - `/proc/uptime` (~20 bytes) for system uptime
fn collect_fast(info: &mut SysInfo) {
    // Load averages
    let mut la = [0u8; 128];
    let lan = raw_read(P_LOADAVG, &mut la);
    let mut parts = la[..lan].split(|&b| b == b' ');
    info.cpu.load_1 = parts.next().map_or(0.0, parse_f64_b);
    info.cpu.load_5 = parts.next().map_or(0.0, parse_f64_b);
    info.cpu.load_15 = parts.next().map_or(0.0, parse_f64_b);

    // CPU frequency (sysfs fast path, /proc/cpuinfo fallback)
    info.cpu.freq = Buf::new();
    let mut fbuf = [0u8; 32];
    let flen = raw_read(P_CPU_FREQ, &mut fbuf);
    if flen > 0 {
        let khz = parse_u64_b(&fbuf[..flen]);
        info.cpu.freq.push_f64_2dp(khz as f64 / 1_000_000.0);
    } else {
        let mut cbuf = [0u8; 2048];
        let cn = raw_read(P_CPUINFO, &mut cbuf);
        if let Some(v) = find_val(&cbuf[..cn], b"cpu MHz", b':') {
            info.cpu.freq.push_f64_2dp(parse_f64_b(v) / 1000.0);
        }
    }

    // Memory
    info.mem.used_gb = Buf::new();
    info.mem.total_gb = Buf::new();
    collect_mem(&mut info.mem);

    // Uptime
    info.login.uptime = Buf::new();
    let mut ubuf = [0u8; 64];
    let un = raw_read(P_UPTIME, &mut ubuf);
    let secs = parse_f64_b(&ubuf[..un]) as u64;
    let days = secs / 86400;
    let hours = (secs % 86400) / 3600;
    let mins = (secs % 3600) / 60;
    if days > 0 {
        info.login.uptime.push_u64(days);
        info.login.uptime.push_str("d ");
    }
    if hours > 0 || days > 0 {
        info.login.uptime.push_u64(hours);
        info.login.uptime.push_str("h ");
    }
    info.login.uptime.push_u64(mins);
    info.login.uptime.push_byte(b'm');
}

// --- Slow data collection ---------------------------------------------------

/// Re-collect slow-changing data (disk usage via statvfs).
/// Called every ~30 seconds (every Nth tick, where N = 30 / interval).
fn collect_slow(info: &mut SysInfo) {
    *info.disk = DiskData {
        label: Buf::new(),
        used: 0,
        total: 1,
        zfs: false,
        health: Buf::new(),
    };
    collect_disk(&mut info.disk);
}

// --- ANSI escape sequences --------------------------------------------------

/// Enter alternate screen buffer, hide cursor, move cursor home.
const ANSI_ENTER: &[u8] = b"\x1b[?1049h\x1b[?25l\x1b[H";
/// Show cursor, leave alternate screen buffer (restores original content).
const ANSI_LEAVE: &[u8] = b"\x1b[?25h\x1b[?1049l";
/// Clear entire screen and move cursor home.
const ANSI_CLEAR: &[u8] = b"\x1b[2J\x1b[H";
/// Move cursor to row 1, column 1.
const ANSI_HOME: &[u8] = b"\x1b[H";

// --- Helpers ----------------------------------------------------------------

/// Write a byte slice to stdout via raw libc::write.
unsafe fn write_stdout(data: &[u8]) {
    unsafe { libc::write(1, data.as_ptr() as *const libc::c_void, data.len()) };
}

// --- Event loop -------------------------------------------------------------

/// Enter the live-updating event loop.
///
/// Sets up epoll with a timerfd (for periodic refresh) and a signalfd (for
/// SIGWINCH resize and SIGINT/SIGTERM exit). Blocks on `epoll_wait` between
/// updates — zero CPU when idle.
///
/// The terminal is put into raw mode (no echo, no canonical input) and an
/// alternate screen buffer is used. All terminal state is restored on exit.
pub fn run_live(info: &mut SysInfo, frame: &mut Frame, mut layout: Layout, interval_s: u64) {
    // Save original terminal settings so we can restore them on exit.
    let mut orig_termios: libc::termios = unsafe { std::mem::zeroed() };
    let have_termios = unsafe { libc::tcgetattr(0, &mut orig_termios) == 0 };
    if have_termios {
        let mut raw = orig_termios;
        // Disable echo (keystrokes invisible) and canonical mode (no
        // line buffering — prevents input from corrupting the display).
        raw.c_lflag &= !(libc::ECHO | libc::ICANON);
        unsafe {
            libc::tcsetattr(0, libc::TCSANOW, &raw);
        }
    }

    unsafe {
        write_stdout(ANSI_ENTER);
    }

    // Write the initial frame (already rendered by main before calling us).
    let mut out = [0u8; 131072];
    let n = frame.write_full(&mut out);
    unsafe {
        write_stdout(&out[..n]);
    }

    // --- Set up epoll with timerfd + signalfd -------------------------------

    let epoll_fd = unsafe { libc::epoll_create1(0) };
    if epoll_fd < 0 {
        unsafe {
            write_stdout(ANSI_LEAVE);
        }
        return;
    }

    // timerfd: fires every `interval_s` seconds for fast data refresh.
    let timer_fd = unsafe { libc::timerfd_create(libc::CLOCK_MONOTONIC, 0) };
    if timer_fd >= 0 {
        let spec = libc::itimerspec {
            it_interval: libc::timespec {
                tv_sec: interval_s as _,
                tv_nsec: 0,
            },
            it_value: libc::timespec {
                tv_sec: interval_s as _,
                tv_nsec: 0,
            },
        };
        unsafe {
            libc::timerfd_settime(timer_fd, 0, &spec, std::ptr::null_mut());
            let mut ev = libc::epoll_event {
                events: libc::EPOLLIN as u32,
                u64: EV_TIMER,
            };
            libc::epoll_ctl(epoll_fd, libc::EPOLL_CTL_ADD, timer_fd, &mut ev);
        }
    }

    // signalfd: delivers SIGWINCH (terminal resize), SIGINT (Ctrl-C), and
    // SIGTERM as readable events instead of interrupting epoll_wait.
    // We block these signals first so they go to signalfd, not the default
    // signal handler.
    let signal_fd = unsafe {
        let mut mask: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut mask);
        libc::sigaddset(&mut mask, libc::SIGWINCH);
        libc::sigaddset(&mut mask, libc::SIGINT);
        libc::sigaddset(&mut mask, libc::SIGTERM);
        libc::sigprocmask(libc::SIG_BLOCK, &mask, std::ptr::null_mut());
        libc::signalfd(-1, &mask, 0)
    };
    if signal_fd >= 0 {
        unsafe {
            let mut ev = libc::epoll_event {
                events: libc::EPOLLIN as u32,
                u64: EV_SIGNAL,
            };
            libc::epoll_ctl(epoll_fd, libc::EPOLL_CTL_ADD, signal_fd, &mut ev);
        }
    }

    // --- Main event loop ----------------------------------------------------

    let mut events = [libc::epoll_event { events: 0, u64: 0 }; 4];
    let mut tick: u64 = 0;
    let slow_every = (30 / interval_s).max(1); // disk refresh interval in ticks

    'ev: loop {
        // Block until a timer tick or signal arrives. Returns immediately
        // when an event is ready; uses zero CPU while waiting.
        let nev =
            unsafe { libc::epoll_wait(epoll_fd, events.as_mut_ptr(), events.len() as i32, -1) };
        if nev < 0 {
            break; // interrupted or error
        }

        let mut full_redraw = false;

        for i in 0..nev as usize {
            match events[i].u64 {
                EV_TIMER => {
                    // Drain the timerfd (must read 8 bytes or it stays readable).
                    let mut tbuf = [0u8; 8];
                    unsafe {
                        libc::read(timer_fd, tbuf.as_mut_ptr() as *mut libc::c_void, 8);
                    }
                    tick += 1;
                    collect_fast(info);
                    if tick % slow_every == 0 {
                        collect_slow(info);
                    }
                }
                EV_SIGNAL => {
                    // Read the signalfd_siginfo struct (128 bytes).
                    // The signal number is the first u32 (ssi_signo).
                    let mut sbuf = [0u8; 128];
                    unsafe {
                        libc::read(signal_fd, sbuf.as_mut_ptr() as *mut libc::c_void, 128);
                    }
                    let signo = u32::from_ne_bytes([sbuf[0], sbuf[1], sbuf[2], sbuf[3]]);
                    match signo as i32 {
                        libc::SIGWINCH => {
                            // Terminal resized: recompute layout and recreate frame.
                            let (tw, th) = crate::sys::terminal_size();
                            layout = compute_layout(info, tw, th);
                            *frame = Frame::new(layout.tw, layout.th);
                            full_redraw = true;
                        }
                        libc::SIGINT | libc::SIGTERM => break 'ev,
                        _ => {}
                    }
                }
                _ => {}
            }
        }

        // --- Re-render and output -------------------------------------------

        // Reset frame cursor and render all cells from scratch.
        frame.row = 0;
        frame.col = 0;
        render(frame, info, &layout);

        // Output: cursor-home then full frame overwrite. On resize, clear
        // the screen first to remove any stale content from the old size.
        if full_redraw {
            unsafe {
                write_stdout(ANSI_CLEAR);
            }
        } else {
            unsafe {
                write_stdout(ANSI_HOME);
            }
        }
        let n = frame.write_full(&mut out);
        unsafe {
            write_stdout(&out[..n]);
        }
    }

    // --- Cleanup: restore terminal ------------------------------------------

    unsafe {
        if have_termios {
            libc::tcsetattr(0, libc::TCSANOW, &orig_termios);
        }
        write_stdout(ANSI_LEAVE);
        if timer_fd >= 0 {
            libc::close(timer_fd);
        }
        if signal_fd >= 0 {
            libc::close(signal_fd);
        }
        libc::close(epoll_fd);
    }
}
