//! Live-updating event loop using epoll + timerfd + signalfd.
//!
//! When `--live` is passed, the report enters an alternate terminal screen
//! and refreshes on a timer. Data collection is tiered:
//!
//! - **Fast** (every tick): load averages, memory, CPU freq, uptime
//! - **Slow** (every ~30s): disk usage via statvfs
//! - **Static** (startup only): OS, kernel, hostname, IPs, CPU model
//!
//! The event loop blocks on `epoll_wait` — zero CPU when idle. SIGWINCH
//! triggers a layout recompute + full redraw. SIGINT/SIGTERM exits cleanly.

use crate::buf::Buf;
use crate::frame::Frame;
use crate::render::{Layout, compute_layout, render};
use crate::sys::{
    DiskData, P_CPU_FREQ, P_UPTIME, SysInfo, collect_disk, collect_mem, find_val, parse_f64_b,
    parse_u64_b, raw_read,
};

const P_CPUINFO: *const u8 = b"/proc/cpuinfo\0".as_ptr();
const P_LOADAVG: *const u8 = b"/proc/loadavg\0".as_ptr();

const EV_TIMER: u64 = 1;
const EV_SIGNAL: u64 = 2;

/// Parse `--live` or `--live=N` from argv. Returns 0 if not in live mode,
/// otherwise the refresh interval in seconds (default 2).
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

/// Re-collect only fast-changing data (~1.7KB total I/O per tick).
fn collect_fast(info: &mut SysInfo) {
    // Load averages — /proc/loadavg (~45 bytes)
    let mut la = [0u8; 128];
    let lan = raw_read(P_LOADAVG, &mut la);
    let mut parts = la[..lan].split(|&b| b == b' ');
    info.cpu.load_1 = parts.next().map_or(0.0, parse_f64_b);
    info.cpu.load_5 = parts.next().map_or(0.0, parse_f64_b);
    info.cpu.load_15 = parts.next().map_or(0.0, parse_f64_b);

    // CPU freq — sysfs scaling_cur_freq (~5 bytes) or /proc/cpuinfo fallback
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

    // Memory — /proc/meminfo (~1.5KB)
    info.mem.used_gb = Buf::new();
    info.mem.total_gb = Buf::new();
    collect_mem(&mut info.mem);

    // Uptime — /proc/uptime (~20 bytes)
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

/// Re-collect slow-changing data (disk usage).
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

/// Enter the live event loop. Takes ownership of the initial frame and layout,
/// updating them in-place on each tick or resize.
pub fn run_live(info: &mut SysInfo, frame: &mut Frame, mut layout: Layout, interval_s: u64) {
    const ENTER: &[u8] = b"\x1b[?1049h\x1b[?25l\x1b[H";
    const LEAVE: &[u8] = b"\x1b[?25h\x1b[?1049l";
    const CLEAR: &[u8] = b"\x1b[2J\x1b[H";
    const HOME: &[u8] = b"\x1b[H";

    // Save termios and disable echo + canonical mode
    let mut orig_termios: libc::termios = unsafe { std::mem::zeroed() };
    let have_termios = unsafe { libc::tcgetattr(0, &mut orig_termios) == 0 };
    if have_termios {
        let mut raw = orig_termios;
        raw.c_lflag &= !(libc::ECHO | libc::ICANON);
        unsafe {
            libc::tcsetattr(0, libc::TCSANOW, &raw);
        }
    }

    // Enter alternate screen + hide cursor
    unsafe {
        libc::write(1, ENTER.as_ptr() as *const libc::c_void, ENTER.len());
    }

    // Initial full render
    let mut out = [0u8; 131072];
    let n = frame.write_full(&mut out);
    unsafe {
        libc::write(1, out.as_ptr() as *const libc::c_void, n);
    }

    // ── Set up epoll with timerfd + signalfd ────────────────────────────

    let epoll_fd = unsafe { libc::epoll_create1(0) };
    if epoll_fd < 0 {
        unsafe {
            libc::write(1, LEAVE.as_ptr() as *const libc::c_void, LEAVE.len());
        }
        return;
    }

    let timer_fd = unsafe { libc::timerfd_create(libc::CLOCK_MONOTONIC, 0) };
    if timer_fd >= 0 {
        let spec = libc::itimerspec {
            it_interval: libc::timespec {
                tv_sec: interval_s as i64,
                tv_nsec: 0,
            },
            it_value: libc::timespec {
                tv_sec: interval_s as i64,
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

    // ── Event loop — blocks on epoll_wait, zero CPU when idle ───────────

    let mut events = [libc::epoll_event { events: 0, u64: 0 }; 4];
    let mut tick: u64 = 0;
    let slow_every = (30 / interval_s).max(1);

    'ev: loop {
        let nev =
            unsafe { libc::epoll_wait(epoll_fd, events.as_mut_ptr(), events.len() as i32, -1) };
        if nev < 0 {
            break;
        }

        let mut full_redraw = false;

        for i in 0..nev as usize {
            match events[i].u64 {
                EV_TIMER => {
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
                    // signalfd_siginfo: ssi_signo is the first u32
                    let mut sbuf = [0u8; 128];
                    unsafe {
                        libc::read(signal_fd, sbuf.as_mut_ptr() as *mut libc::c_void, 128);
                    }
                    let signo = u32::from_ne_bytes([sbuf[0], sbuf[1], sbuf[2], sbuf[3]]);
                    match signo as i32 {
                        libc::SIGWINCH => {
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

        // Re-render: reset cursor to frame origin, fill all cells
        frame.row = 0;
        frame.col = 0;
        render(frame, info, &layout);

        // Cursor-home + full overwrite (robust, ~3.5KB per tick)
        if full_redraw {
            unsafe {
                libc::write(1, CLEAR.as_ptr() as *const libc::c_void, CLEAR.len());
            }
        } else {
            unsafe {
                libc::write(1, HOME.as_ptr() as *const libc::c_void, HOME.len());
            }
        }
        let n = frame.write_full(&mut out);
        unsafe {
            libc::write(1, out.as_ptr() as *const libc::c_void, n);
        }
    }

    // ── Cleanup: restore terminal ───────────────────────────────────────

    unsafe {
        if have_termios {
            libc::tcsetattr(0, libc::TCSANOW, &orig_termios);
        }
        libc::write(1, LEAVE.as_ptr() as *const libc::c_void, LEAVE.len());
        if timer_fd >= 0 {
            libc::close(timer_fd);
        }
        if signal_fd >= 0 {
            libc::close(signal_fd);
        }
        libc::close(epoll_fd);
    }
}
