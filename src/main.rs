//! mstat — zero-allocation Linux system information display.
//!
//! Reads directly from /proc, /sys, utmp, and systemd session files.
//!
//! Copyright 2026, Joshua Finley. BSD-3-Clause License.
//!
//! # Usage
//!
//! ```text
//! mstat              One-shot report to stdout
//! mstat --live       Live-updating TUI (2s default)
//! mstat --live=5     Live-updating TUI (5s interval)
//! ```

mod buf;
mod frame;
mod live;
mod render;
mod sys;

use frame::Frame;
use live::{parse_live_interval, run_live};
use render::{compute_layout, render};
use sys::{SysInfo, collect, terminal_size};

fn main() {
    let live_interval = parse_live_interval();

    let mut info = SysInfo::new();
    collect(&mut info);

    let (tw, th) = terminal_size();
    let layout = compute_layout(&info, tw, th);

    let mut frame = Frame::new(layout.tw, layout.th);
    render(&mut frame, &info, &layout);

    if live_interval > 0 {
        run_live(&mut info, &mut frame, layout, live_interval);
    } else {
        // One-shot: single write() syscall, with trailing newline for shell
        let mut out = [0u8; 65536];
        let mut n = frame.write_full(&mut out);
        out[n] = b'\n';
        n += 1;
        unsafe {
            libc::write(1, out.as_ptr() as *const libc::c_void, n);
        }
    }
}
