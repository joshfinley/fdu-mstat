//! Layout computation and report rendering.
//!
//! The layout adapts to the terminal size: if the terminal is narrower than
//! 119 columns or shorter than 79 rows, the report expands to fill the
//! available space.

use crate::buf::Buf;
use crate::frame::{DivStyle, Frame};
use crate::sys::SysInfo;

// -- Layout constants --------------------------------------------------------

pub const MAX_TERM_W: usize = 119;
pub const MAX_TERM_H: usize = 79;
const DEFAULT_W: usize = 52;
const NAME_COL: usize = 13;
const BORDER_PAD: usize = 7; // "│ " + " │ " + " │"

const TITLE: &str = "FOSL DISTUTILS";
const SUBTITLE: &str = "MACHINE REPORT";

// -- Layout ------------------------------------------------------------------

pub struct Layout {
    pub nc: usize,  // name column width
    pub dc: usize,  // data column width
    pub tw: usize,  // total frame width
    pub th: usize,  // total frame height
    pub fill: bool, // whether to pad with empty rows
}

/// Count the content rows the report will produce (excluding vertical fill).
fn count_rows(info: &SysInfo) -> usize {
    let dns = info.net.dns_count;
    let zfs: usize = if info.disk.zfs { 1 } else { 0 };
    let lip: usize = if info.login.has_ip { 1 } else { 0 };
    1 + 2 + 1 + 2 + 1 + 4 + dns + 1 + 7 + 1 + 2 + zfs + 1 + 2 + 1 + 1 + lip + 1 + 1
}

/// Compute frame dimensions from terminal size and data content widths.
pub fn compute_layout(info: &SysInfo, tw: usize, th: usize) -> Layout {
    let strs: [usize; 13] = [
        TITLE.len(),
        SUBTITLE.len(),
        info.os.name.char_count(),
        info.os.kernel.char_count(),
        info.net.hostname.char_count(),
        info.net.machine_ip.char_count(),
        info.net.client_ip.char_count(),
        info.net.user.char_count(),
        info.cpu.model.char_count(),
        info.cpu.hypervisor.char_count(),
        info.disk.label.char_count(),
        info.login.time.char_count(),
        info.login.uptime.char_count(),
    ];
    let min_data = strs.iter().copied().max().unwrap_or(20).max(20);
    let min_total = NAME_COL + min_data + BORDER_PAD;

    let total_w = if tw > 0 && tw < MAX_TERM_W {
        tw.max(min_total)
    } else {
        min_total.max(DEFAULT_W)
    };
    let dc = total_w.saturating_sub(NAME_COL + BORDER_PAD);
    let content_rows = count_rows(info);
    let total_h = if th > 0 && th < MAX_TERM_H {
        th.max(content_rows)
    } else {
        content_rows
    };
    let fill = th > 0 && th < MAX_TERM_H && total_h > content_rows;

    Layout {
        nc: NAME_COL,
        dc,
        tw: total_w,
        th: total_h,
        fill,
    }
}

// -- Render ------------------------------------------------------------------

/// Fill the frame with the complete machine report from collected system data.
pub fn render(frame: &mut Frame, info: &SysInfo, layout: &Layout) {
    let nc = layout.nc;
    let dc = layout.dc;

    frame.put_header();
    frame.put_centered(TITLE);
    frame.put_centered(SUBTITLE);

    // OS
    frame.put_divider(DivStyle::Top, nc);
    frame.put_row_buf("OS", &info.os.name, nc, dc);
    frame.put_row_buf("KERNEL", &info.os.kernel, nc, dc);

    // Network
    frame.put_divider(DivStyle::Mid, nc);
    frame.put_row_buf("HOSTNAME", &info.net.hostname, nc, dc);
    frame.put_row_buf("MACHINE IP", &info.net.machine_ip, nc, dc);
    frame.put_row_buf("CLIENT  IP", &info.net.client_ip, nc, dc);
    for i in 0..info.net.dns_count {
        let mut label = Buf::<16>::new();
        label.push_str("DNS  IP ");
        label.push_u64((i + 1) as u64);
        frame.put_row(label.as_str(), info.net.dns[i].as_str(), nc, dc);
    }
    frame.put_row_buf("USER", &info.net.user, nc, dc);

    // CPU
    frame.put_divider(DivStyle::Mid, nc);
    frame.put_row_buf("PROCESSOR", &info.cpu.model, nc, dc);
    {
        let mut cores = Buf::<64>::new();
        cores.push_bytes(info.cpu.cores_per_socket.as_bytes());
        cores.push_str(" vCPU(s) / ");
        cores.push_bytes(info.cpu.sockets.as_bytes());
        cores.push_str(" Socket(s)");
        frame.put_row("CORES", cores.as_str(), nc, dc);
    }
    frame.put_row_buf("HYPERVISOR", &info.cpu.hypervisor, nc, dc);
    {
        let mut freq = Buf::<32>::new();
        freq.push_bytes(info.cpu.freq.as_bytes());
        freq.push_str(" GHz");
        frame.put_row("CPU FREQ", freq.as_str(), nc, dc);
    }
    frame.put_bar_row("LOAD  1m", info.cpu.load_1, info.cpu.total_cores, nc, dc);
    frame.put_bar_row("LOAD  5m", info.cpu.load_5, info.cpu.total_cores, nc, dc);
    frame.put_bar_row("LOAD 15m", info.cpu.load_15, info.cpu.total_cores, nc, dc);

    // Disk
    frame.put_divider(DivStyle::Mid, nc);
    frame.put_row_buf("VOLUME", &info.disk.label, nc, dc);
    frame.put_bar_row(
        "DISK USAGE",
        info.disk.used as f64,
        info.disk.total as f64,
        nc,
        dc,
    );
    if info.disk.zfs {
        frame.put_row_buf("ZFS HEALTH", &info.disk.health, nc, dc);
    }

    // Memory
    frame.put_divider(DivStyle::Mid, nc);
    {
        let mut mem = Buf::<64>::new();
        mem.push_bytes(info.mem.used_gb.as_bytes());
        mem.push_byte(b'/');
        mem.push_bytes(info.mem.total_gb.as_bytes());
        mem.push_str(" GiB [");
        mem.push_f64_2dp(info.mem.percent);
        mem.push_str("%]");
        frame.put_row("MEMORY", mem.as_str(), nc, dc);
    }
    frame.put_bar_row(
        "USAGE",
        info.mem.used_kb as f64,
        info.mem.total_kb as f64,
        nc,
        dc,
    );

    // Login / Uptime
    frame.put_divider(DivStyle::Mid, nc);
    frame.put_row_buf("LAST LOGIN", &info.login.time, nc, dc);
    if info.login.has_ip {
        frame.put_row_buf("", &info.login.ip, nc, dc);
    }
    frame.put_row_buf("UPTIME", &info.login.uptime, nc, dc);

    // Vertical fill for small terminals
    if layout.fill {
        let target = layout.th - 1;
        while frame.row < target {
            frame.put_empty_row(nc, dc);
        }
    }

    frame.put_divider(DivStyle::Bot, nc);
}
