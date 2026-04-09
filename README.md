# mstat

A zero-allocation, near-zero-subprocess system information display for Linux,
written in Rust. Designed for MOTD banners, dashboards, and live monitoring.

Visuals inspired by [usgc machine report](https://github.com/usgraphics/usgc-machine-report).

## Usage

```sh
mstat              # one-shot report to stdout
mstat --live       # live TUI, 2s refresh (Ctrl-C to exit)
mstat --live=5     # live TUI, 5s refresh
```

## Platform support

**Linux only.** This tool reads directly from Linux-specific interfaces:

- `/proc/cpuinfo`, `meminfo`, `loadavg`, `uptime` — CPU, memory, load, uptime
- `/sys/devices/cpu/*/cpufreq/scaling_cur_freq` — live CPU frequency
- `/proc/mounts` — filesystem detection (ZFS)
- `utmp` (`/var/run/utmp`, `/run/utmp`) — login records, client IP
- `/run/systemd/sessions/*` — login fallback on systemd systems
- `epoll` + `timerfd` + `signalfd` — live mode event loop
- `statvfs(2)` — disk usage

BSD/macOS would require `sysctl` for hardware info, `kqueue` for the event
loop, and POSIX `utmpx` for login records. These are not currently
implemented, but would make for great contributions. The `SysInfo` struct 
and rendering pipeline are platform-agnostic; only the collectors in `sys.rs` 
and the event loop in `live.rs` would need `cfg`-gated alternatives.

## Performance

All data is read directly from `/proc`, `/sys`, `utmp`, and systemd session
files via raw `libc::open`/`read`/`close` calls into stack buffers. Zero heap
allocation in the data and render path. The only subprocess fallback is for
ZFS health (fires only when ZFS is detected in `/proc/mounts`).

### Benchmarks

All measurements via `perf stat`, one-shot mode. musl builds are statically
linked (no dynamic linker overhead).

**Intel i5-4300U Haswell, Debian 13, glibc:**

```text
  1.26 msec task-clock    127 page-faults    2,842,384 cycles
  2,494,845 instructions  (0.88 IPC)         16,189 branch-misses
```

**QEMU/KVM x86_64, openSUSE Leap 16, musl (static):**

```text
  0.79 msec task-clock     72 page-faults    1,986,193 cycles
  2,114,984 instructions  (1.06 IPC)
```

**Raspberry Pi CM4 (aarch64), Debian 12, glibc:**

```text
  2.10 msec task-clock    131 page-faults      (BCM2711 @ 1.5 GHz)
```

At these timescales, the ELF loader and libc/kernel init are a meaningful
fraction of total execution time. musl eliminates ~55 page faults by
removing the dynamic linker entirely, pushing below 1ms on most hardware.

This is a **>230x speedup** over the original bash implementation (291ms),
which spawned ~15 subprocesses (`lscpu`, `who`, `lastlog`, `grep`, `awk`,
`df`, subshells for bar graphs, etc.). The Rust version spawns zero
subprocesses and makes zero heap allocations in the data/render path.

## Building

```sh
# Host build
cargo build --release

# Cross-compile (requires `cross` + podman)
cargo install cross
make bundle              # builds all targets, tarballs in dist/
make release TARGET=aarch64-unknown-linux-gnu  # single target
```

Cross-compilation uses [cross](https://github.com/cross-rs/cross) with
podman as the container engine (set via `CROSS_CONTAINER_ENGINE=podman` in
the Makefile). Docker works too — override with
`make bundle CROSS_CONTAINER_ENGINE=docker`.

**Note:** run `cargo clean` before cross-compiling if you previously built
for the host, otherwise cached build scripts may fail with glibc version
mismatches.

Release artifacts are placed in `dist/` with SHA256 checksums.

### Targets

The Makefile supports these Linux triples:

- `x86_64-unknown-linux-gnu` / `musl`
- `aarch64-unknown-linux-gnu` / `musl`
- `armv7-unknown-linux-gnueabihf`
- `riscv64gc-unknown-linux-gnu`

## Architecture

```
src/
├── main.rs     Entry point and arg dispatch
├── buf.rs      Buf<N> — fixed-size stack-allocated string buffer
├── sys.rs      System data structs, collectors, raw I/O, parsing
├── frame.rs    Cell-based frame buffer with box-drawing primitives
├── render.rs   Layout computation and report rendering
└── live.rs     epoll + timerfd + signalfd event loop for --live mode
```

Data collection is tiered for live mode:

- **Static** (startup only) — OS, kernel, hostname, IPs, CPU model/topology
- **Fast** (every tick) — /proc/loadavg, meminfo, uptime, sysfs freq
- **Slow** (~30s) — statvfs (disk usage)

## License

BSD-3-Clause. Copyright 2026, Joshua Finley.
