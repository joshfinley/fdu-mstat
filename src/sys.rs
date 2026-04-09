//! System data collection — reads /proc, /sys, utmp, and systemd sessions.
//!
//! All collectors write into stack-allocated `Buf<N>` fields. No heap
//! allocation occurs. The only subprocess fallback is ZFS (zfs/zpool
//! commands), which fires only when ZFS filesystems are detected.

use crate::cache_padded::CachePadded;

use crate::buf::Buf;

// -- Null-terminated paths for libc::open ------------------------------------

const P_CPUINFO: *const u8 = b"/proc/cpuinfo\0".as_ptr();
const P_LOADAVG: *const u8 = b"/proc/loadavg\0".as_ptr();
const P_MEMINFO: *const u8 = b"/proc/meminfo\0".as_ptr();
pub const P_UPTIME: *const u8 = b"/proc/uptime\0".as_ptr();
const P_OSREL: *const u8 = b"/etc/os-release\0".as_ptr();
const P_RESOLV: *const u8 = b"/etc/resolv.conf\0".as_ptr();
const P_MOUNTS: *const u8 = b"/proc/mounts\0".as_ptr();
const P_UTMP1: *const u8 = b"/var/run/utmp\0".as_ptr();
const P_UTMP2: *const u8 = b"/run/utmp\0".as_ptr();
const P_DMI_VENDOR: *const u8 = b"/sys/class/dmi/id/sys_vendor\0".as_ptr();
const P_ROOT: *const u8 = b"/\0".as_ptr();
pub const P_CPU_FREQ: *const u8 =
    b"/sys/devices/system/cpu/cpu0/cpufreq/scaling_cur_freq\0".as_ptr();
const P_SD_SESSIONS: *const u8 = b"/run/systemd/sessions\0".as_ptr();
const P_DT_MODEL: *const u8 = b"/sys/firmware/devicetree/base/model\0".as_ptr();

const MONTHS: [&[u8]; 12] = [
    b"Jan", b"Feb", b"Mar", b"Apr", b"May", b"Jun", b"Jul", b"Aug", b"Sep", b"Oct", b"Nov", b"Dec",
];

// -- Raw I/O -----------------------------------------------------------------

/// Read a file into `buf` via libc::open/read/close. Returns bytes read.
/// Path must be null-terminated.
#[inline]
pub fn raw_read(path: *const u8, buf: &mut [u8]) -> usize {
    unsafe {
        let fd = libc::open(path as *const libc::c_char, libc::O_RDONLY);
        if fd < 0 {
            return 0;
        }
        let n = libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len());
        libc::close(fd);
        if n > 0 { n as usize } else { 0 }
    }
}

/// Read utmp entries directly into a properly-aligned array.
fn raw_read_entries(path: *const u8, entries: &mut [UtmpEntry]) -> usize {
    unsafe {
        let fd = libc::open(path as *const libc::c_char, libc::O_RDONLY);
        if fd < 0 {
            return 0;
        }
        let byte_cap = entries.len() * std::mem::size_of::<UtmpEntry>();
        let n = libc::read(fd, entries.as_mut_ptr() as *mut libc::c_void, byte_cap);
        libc::close(fd);
        if n > 0 {
            n as usize / std::mem::size_of::<UtmpEntry>()
        } else {
            0
        }
    }
}

// -- Parsing helpers ---------------------------------------------------------

/// Parse a `u64` from the first decimal run in a byte slice, skipping
/// leading whitespace.
pub fn parse_u64_b(s: &[u8]) -> u64 {
    let mut r: u64 = 0;
    let mut started = false;
    for &b in s {
        if b.is_ascii_digit() {
            started = true;
            r = r * 10 + (b - b'0') as u64;
        } else if started {
            break;
        }
    }
    r
}

/// Parse an `f64` from a byte slice (handles sign, decimal point).
pub fn parse_f64_b(s: &[u8]) -> f64 {
    let mut result: f64 = 0.0;
    let mut decimal = false;
    let mut place = 0.1;
    let mut neg = false;
    for &b in s {
        match b {
            b'-' if !decimal => neg = true,
            b'.' => decimal = true,
            b'0'..=b'9' => {
                if decimal {
                    result += (b - b'0') as f64 * place;
                    place *= 0.1;
                } else {
                    result = result * 10.0 + (b - b'0') as f64;
                }
            }
            _ if decimal || result > 0.0 => break,
            _ => {}
        }
    }
    if neg { -result } else { result }
}

/// Find a "key<sep>value" line in a multi-line byte buffer and return the
/// trimmed value. Works for `/proc` style (`key\t: value`) and os-release
/// style (`KEY=value` / `KEY="value"`).
pub fn find_val<'a>(content: &'a [u8], key: &[u8], sep: u8) -> Option<&'a [u8]> {
    for line in content.split(|&b| b == b'\n') {
        if line.len() > key.len() && line.starts_with(key) {
            let next = line[key.len()];
            if next == sep || next == b'\t' || next == b' ' {
                if let Some(pos) = line.iter().position(|&b| b == sep) {
                    let val = &line[pos + 1..];
                    let start = val
                        .iter()
                        .position(|&b| b != b' ' && b != b'\t' && b != b'"')
                        .unwrap_or(val.len());
                    let end = val
                        .iter()
                        .rposition(|&b| b != b' ' && b != b'\t' && b != b'\r' && b != b'"')
                        .map_or(start, |e| e + 1);
                    if start < end {
                        return Some(&val[start..end]);
                    }
                }
            }
        }
    }
    None
}

/// Length of a C string (up to the first null byte).
fn cstr_len(buf: &[u8]) -> usize {
    buf.iter().position(|&b| b == 0).unwrap_or(buf.len())
}

/// Copy `src` into `dst`, capitalizing the first ASCII letter.
fn capitalize_into<const N: usize>(src: &[u8], dst: &mut Buf<N>) {
    if src.is_empty() {
        return;
    }
    if src[0].is_ascii_lowercase() {
        dst.push_byte(src[0] - 32);
        dst.push_bytes(&src[1..]);
    } else {
        dst.push_bytes(src);
    }
}

/// Format a Unix timestamp as "Mon DD YYYY HH:MM".
fn format_timestamp<const N: usize>(ts: i64, buf: &mut Buf<N>) {
    unsafe {
        let time: libc::time_t = ts;
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&time, &mut tm);
        buf.push_bytes(MONTHS[(tm.tm_mon as usize) % 12]);
        buf.push_byte(b' ');
        buf.push_u64(tm.tm_mday as u64);
        buf.push_byte(b' ');
        buf.push_u64((tm.tm_year + 1900) as u64);
        buf.push_byte(b' ');
        if tm.tm_hour < 10 {
            buf.push_byte(b'0');
        }
        buf.push_u64(tm.tm_hour as u64);
        buf.push_byte(b':');
        if tm.tm_min < 10 {
            buf.push_byte(b'0');
        }
        buf.push_u64(tm.tm_min as u64);
    }
}

// -- Terminal size -----------------------------------------------------------

/// Query the terminal dimensions via ioctl(TIOCGWINSZ).
/// Returns (0, 0) if stdout is not a terminal.
pub fn terminal_size() -> (usize, usize) {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 && ws.ws_row > 0 {
            (ws.ws_col as usize, ws.ws_row as usize)
        } else {
            (0, 0)
        }
    }
}

// -- Utmp (login records) ----------------------------------------------------

/// Linux utmp record layout (x86_64). 384 bytes per entry.
#[repr(C)]
struct UtmpEntry {
    ut_type: i16,
    _pad: i16,
    ut_pid: i32,
    ut_line: [u8; 32],
    ut_id: [u8; 4],
    ut_user: [u8; 32],
    ut_host: [u8; 256],
    ut_exit: [i16; 2],
    ut_session: i32,
    ut_tv_sec: i32,
    ut_tv_usec: i32,
    ut_addr_v6: [i32; 4],
    __reserved: [u8; 20],
}

const _: () = assert!(std::mem::size_of::<UtmpEntry>() == 384);
const UT_USER_PROCESS: i16 = 7;

/// Get the device name of the current tty (stripped of "/dev/" prefix).
fn get_current_tty() -> [u8; 32] {
    let mut buf = [0u8; 32];
    unsafe {
        let tty = libc::ttyname(0);
        if !tty.is_null() {
            let s = std::ffi::CStr::from_ptr(tty).to_bytes();
            let stripped = if s.starts_with(b"/dev/") { &s[5..] } else { s };
            let len = stripped.len().min(31);
            buf[..len].copy_from_slice(&stripped[..len]);
        }
    }
    buf
}

// -- Cache-padded data sections ----------------------------------------------
//
// Each data group is wrapped in CachePadded (128-byte aligned on x86_64 /
// aarch64, 64-byte on others) to eliminate false sharing if collectors are
// ever parallelized, and to improve prefetch locality during rendering.

pub struct OsData {
    pub name: Buf<128>,
    pub kernel: Buf<128>,
}

pub struct NetData {
    pub hostname: Buf<128>,
    pub machine_ip: Buf<64>,
    pub client_ip: Buf<64>,
    pub dns: [Buf<64>; 8],
    pub dns_count: usize,
    pub user: Buf<64>,
}

pub struct CpuData {
    pub model: Buf<128>,
    pub cores_per_socket: Buf<16>,
    pub sockets: Buf<16>,
    pub hypervisor: Buf<64>,
    pub freq: Buf<16>,
    pub load_1: f64,
    pub load_5: f64,
    pub load_15: f64,
    pub total_cores: f64,
}

pub struct DiskData {
    pub label: Buf<64>,
    pub used: u64,
    pub total: u64,
    pub zfs: bool,
    pub health: Buf<32>,
}

pub struct MemData {
    pub used_kb: u64,
    pub total_kb: u64,
    pub percent: f64,
    pub used_gb: Buf<16>,
    pub total_gb: Buf<16>,
}

pub struct LoginData {
    pub time: Buf<64>,
    pub ip: Buf<64>,
    pub has_ip: bool,
    pub uptime: Buf<32>,
}

pub struct SysInfo {
    pub os: CachePadded<OsData>,
    pub net: CachePadded<NetData>,
    pub cpu: CachePadded<CpuData>,
    pub disk: CachePadded<DiskData>,
    pub mem: CachePadded<MemData>,
    pub login: CachePadded<LoginData>,
}

impl SysInfo {
    pub fn new() -> Self {
        Self {
            os: CachePadded::new(OsData {
                name: Buf::new(),
                kernel: Buf::new(),
            }),
            net: CachePadded::new(NetData {
                hostname: Buf::new(),
                machine_ip: Buf::new(),
                client_ip: Buf::new(),
                dns: [Buf::new(); 8],
                dns_count: 0,
                user: Buf::new(),
            }),
            cpu: CachePadded::new(CpuData {
                model: Buf::new(),
                cores_per_socket: Buf::new(),
                sockets: Buf::new(),
                hypervisor: Buf::new(),
                freq: Buf::new(),
                load_1: 0.0,
                load_5: 0.0,
                load_15: 0.0,
                total_cores: 1.0,
            }),
            disk: CachePadded::new(DiskData {
                label: Buf::new(),
                used: 0,
                total: 1,
                zfs: false,
                health: Buf::new(),
            }),
            mem: CachePadded::new(MemData {
                used_kb: 0,
                total_kb: 1,
                percent: 0.0,
                used_gb: Buf::new(),
                total_gb: Buf::new(),
            }),
            login: CachePadded::new(LoginData {
                time: Buf::new(),
                ip: Buf::new(),
                has_ip: false,
                uptime: Buf::new(),
            }),
        }
    }
}

// -- Collectors --------------------------------------------------------------

/// Collect OS name from /etc/os-release and kernel from uname(2).
fn collect_os(data: &mut OsData) {
    let mut buf = [0u8; 2048];
    let n = raw_read(P_OSREL, &mut buf);
    let content = &buf[..n];

    let id = find_val(content, b"ID", b'=').unwrap_or(b"linux");
    let ver = find_val(content, b"VERSION", b'=').unwrap_or(b"");
    let code = find_val(content, b"VERSION_CODENAME", b'=').unwrap_or(b"");

    capitalize_into(id, &mut data.name);
    data.name.push_byte(b' ');
    data.name.push_bytes(ver);
    data.name.push_byte(b' ');
    capitalize_into(code, &mut data.name);

    unsafe {
        let mut u: libc::utsname = std::mem::zeroed();
        if libc::uname(&mut u) == 0 {
            let sn = std::ffi::CStr::from_ptr(u.sysname.as_ptr()).to_bytes();
            let rel = std::ffi::CStr::from_ptr(u.release.as_ptr()).to_bytes();
            data.kernel.push_bytes(sn);
            data.kernel.push_byte(b' ');
            data.kernel.push_bytes(rel);
        }
    }
}

/// Collect network info: hostname (gethostname), machine IP (UDP socket
/// trick), DNS servers (/etc/resolv.conf), and current user ($USER).
fn collect_net(data: &mut NetData, user_bytes: &[u8]) {
    // Hostname
    unsafe {
        let mut raw = [0u8; 256];
        if libc::gethostname(raw.as_mut_ptr() as *mut libc::c_char, raw.len()) == 0 {
            let len = cstr_len(&raw);
            if len > 0 {
                data.hostname.push_bytes(&raw[..len]);
            }
        }
    }
    if data.hostname.is_empty() {
        data.hostname.push_str("Not Defined");
    }

    // Machine IP — connect a UDP socket to 8.8.8.8:53 (never sends data),
    // then read back the local address via getsockname().
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0);
        if fd >= 0 {
            let mut addr: libc::sockaddr_in = std::mem::zeroed();
            addr.sin_family = libc::AF_INET as u16;
            addr.sin_port = 53u16.to_be();
            addr.sin_addr.s_addr = u32::from_ne_bytes([8, 8, 8, 8]);
            let ret = libc::connect(
                fd,
                &addr as *const _ as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            );
            if ret == 0 {
                let mut local: libc::sockaddr_in = std::mem::zeroed();
                let mut len: libc::socklen_t =
                    std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
                libc::getsockname(fd, &mut local as *mut _ as *mut libc::sockaddr, &mut len);
                let ip = local.sin_addr.s_addr.to_ne_bytes();
                data.machine_ip.push_u64(ip[0] as u64);
                data.machine_ip.push_byte(b'.');
                data.machine_ip.push_u64(ip[1] as u64);
                data.machine_ip.push_byte(b'.');
                data.machine_ip.push_u64(ip[2] as u64);
                data.machine_ip.push_byte(b'.');
                data.machine_ip.push_u64(ip[3] as u64);
            }
            libc::close(fd);
        }
    }
    if data.machine_ip.is_empty() {
        data.machine_ip.push_str("No IP found");
    }

    // DNS from /etc/resolv.conf
    let mut rbuf = [0u8; 4096];
    let rn = raw_read(P_RESOLV, &mut rbuf);
    for line in rbuf[..rn].split(|&b| b == b'\n') {
        if data.dns_count >= 8 {
            break;
        }
        if line.starts_with(b"nameserver") {
            let rest = &line[10..];
            let start = rest
                .iter()
                .position(|&b| b != b' ' && b != b'\t')
                .unwrap_or(rest.len());
            let end = rest[start..]
                .iter()
                .position(|&b| b == b' ' || b == b'\t' || b == b'\r')
                .map_or(rest.len(), |p| start + p);
            if start < end {
                data.dns[data.dns_count].push_bytes(&rest[start..end]);
                data.dns_count += 1;
            }
        }
    }

    // User
    unsafe {
        let u = libc::getenv(b"USER\0".as_ptr() as *const libc::c_char);
        if !u.is_null() {
            let s = std::ffi::CStr::from_ptr(u).to_bytes();
            data.user.push_bytes(s);
        } else {
            data.user.push_bytes(user_bytes);
        }
    }
}

/// Collect CPU info from /proc/cpuinfo: model, core/socket topology,
/// frequency, hypervisor detection, and load averages.
///
/// x86 and ARM have different /proc/cpuinfo layouts:
///   x86:  "model name", "cpu MHz", "cpu cores", "physical id"
///   ARM:  "Model" (at end of file), no MHz/cores/physical id
///
/// Fallbacks: sysfs for frequency, devicetree for model name, and
/// processor line count for core count.
fn collect_cpu(data: &mut CpuData) {
    let mut buf = [0u8; 16384];
    let n = raw_read(P_CPUINFO, &mut buf);
    let content = &buf[..n];

    // Model name: x86 "model name", ARM "Model", then devicetree fallback
    if let Some(v) = find_val(content, b"model name", b':') {
        data.model.push_bytes(v);
    } else if let Some(v) = find_val(content, b"Model", b':') {
        data.model.push_bytes(v);
    } else {
        // ARM devicetree: /sys/firmware/devicetree/base/model
        let mut mbuf = [0u8; 256];
        let mn = raw_read(P_DT_MODEL, &mut mbuf);
        if mn > 0 {
            // Strip trailing null byte if present
            let end = cstr_len(&mbuf[..mn]);
            data.model.push_bytes(&mbuf[..end]);
        }
    }

    // CPU frequency: x86 "cpu MHz", fallback to sysfs scaling_cur_freq
    if let Some(v) = find_val(content, b"cpu MHz", b':') {
        data.freq.push_f64_2dp(parse_f64_b(v) / 1000.0);
    } else {
        let mut fbuf = [0u8; 32];
        let flen = raw_read(P_CPU_FREQ, &mut fbuf);
        if flen > 0 {
            let khz = parse_u64_b(&fbuf[..flen]);
            data.freq.push_f64_2dp(khz as f64 / 1_000_000.0);
        }
    }

    // Cores per socket: x86 "cpu cores", ARM fallback to processor count
    let has_cpu_cores = find_val(content, b"cpu cores", b':');
    if let Some(v) = has_cpu_cores {
        data.cores_per_socket.push_bytes(v);
    }

    // Count unique physical IDs for socket count; count processor lines for CPU count.
    // On ARM, physical id doesn't exist: socket_count stays 1, cpu_count = processor lines.
    let mut seen = [false; 256];
    let mut socket_count: u64 = 0;
    let mut cpu_count: u64 = 0;
    for line in content.split(|&b| b == b'\n') {
        if line.starts_with(b"processor") {
            cpu_count += 1;
        }
        if line.starts_with(b"physical id") {
            if let Some(v) = line.iter().position(|&b| b == b':') {
                let id = parse_u64_b(&line[v + 1..]) as usize;
                if id < 256 && !seen[id] {
                    seen[id] = true;
                    socket_count += 1;
                }
            }
        }
    }
    if socket_count == 0 {
        socket_count = 1;
    }
    if cpu_count == 0 {
        cpu_count = 1;
    }
    // If "cpu cores" wasn't in cpuinfo, use total processor count
    if has_cpu_cores.is_none() {
        data.cores_per_socket.push_u64(cpu_count);
    }
    data.sockets.push_u64(socket_count);
    data.total_cores = cpu_count as f64;

    // Hypervisor: check /proc/cpuinfo flags for "hypervisor", then identify
    // the vendor from /sys/class/dmi/id/sys_vendor.
    let is_virtual = content
        .windows(12)
        .any(|w| w == b" hypervisor " || w == b" hypervisor\n");
    if is_virtual {
        let mut vbuf = [0u8; 256];
        let vn = raw_read(P_DMI_VENDOR, &mut vbuf);
        if vn > 0 {
            let vendor = &vbuf[..vn];
            if vendor.windows(6).any(|w| w == b"VMware") {
                data.hypervisor.push_str("VMware");
            } else if vendor.windows(4).any(|w| w == b"QEMU") {
                data.hypervisor.push_str("KVM/QEMU");
            } else if vendor.windows(9).any(|w| w == b"Microsoft") {
                data.hypervisor.push_str("Hyper-V");
            } else if vendor.windows(3).any(|w| w == b"Xen") {
                data.hypervisor.push_str("Xen");
            } else if vendor.windows(6).any(|w| w == b"Amazon") {
                data.hypervisor.push_str("Amazon/KVM");
            } else if vendor.windows(6).any(|w| w == b"Google") {
                data.hypervisor.push_str("Google Compute");
            } else {
                data.hypervisor.push_str("Virtual");
            }
        } else {
            data.hypervisor.push_str("Virtual");
        }
    } else {
        data.hypervisor.push_str("Bare Metal");
    }

    // Load averages from /proc/loadavg
    let mut la = [0u8; 128];
    let lan = raw_read(P_LOADAVG, &mut la);
    let mut parts = la[..lan].split(|&b| b == b' ');
    data.load_1 = parts.next().map_or(0.0, parse_f64_b);
    data.load_5 = parts.next().map_or(0.0, parse_f64_b);
    data.load_15 = parts.next().map_or(0.0, parse_f64_b);
}

/// Collect disk usage via statvfs(2). Falls back to `zfs`/`zpool` subprocess
/// only when ZFS is detected in /proc/mounts.
pub fn collect_disk(data: &mut DiskData) {
    let mut mbuf = [0u8; 8192];
    let mn = raw_read(P_MOUNTS, &mut mbuf);
    let has_zfs = mbuf[..mn].windows(3).any(|w| w == b"zfs");

    if has_zfs {
        if let Ok(out) = std::process::Command::new("zfs")
            .args([
                "get",
                "-o",
                "value",
                "-Hp",
                "used,available",
                "zroot/ROOT/os",
            ])
            .output()
        {
            if out.status.success() {
                let mut lines = out.stdout.split(|&b| b == b'\n');
                let used = lines.next().map_or(0, |l| parse_u64_b(l));
                let avail = lines.next().map_or(0, |l| parse_u64_b(l));
                let total = used + avail;
                let pct = if total > 0 {
                    (used as f64 / total as f64) * 100.0
                } else {
                    0.0
                };
                data.label.push_f64_2dp(used as f64 / 1_073_741_824.0);
                data.label.push_byte(b'/');
                data.label.push_f64_2dp(avail as f64 / 1_073_741_824.0);
                data.label.push_str(" GB [");
                data.label.push_f64_2dp(pct);
                data.label.push_str("%]");
                data.used = used;
                data.total = total;
                data.zfs = true;

                if let Ok(health_out) = std::process::Command::new("zpool")
                    .args(["status", "-x", "zroot"])
                    .output()
                {
                    if health_out.stdout.windows(10).any(|w| w == b"is healthy") {
                        data.health.push_str("HEALTH O.K.");
                    } else {
                        data.health.push_str("DEGRADED");
                    }
                }
                return;
            }
        }
    }

    // Non-ZFS: statvfs on /
    unsafe {
        let mut stat: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(P_ROOT as *const libc::c_char, &mut stat) == 0 {
            let block = stat.f_frsize as u64;
            let total_bytes = stat.f_blocks as u64 * block;
            let used_bytes = total_bytes - (stat.f_bfree as u64 * block);
            let total_mb = total_bytes / (1024 * 1024);
            let used_mb = used_bytes / (1024 * 1024);
            let pct = if total_bytes > 0 {
                (used_bytes as f64 / total_bytes as f64) * 100.0
            } else {
                0.0
            };
            data.label.push_f64_2dp(used_mb as f64 / 1024.0);
            data.label.push_byte(b'/');
            data.label.push_f64_2dp(total_mb as f64 / 1024.0);
            data.label.push_str(" GB [");
            data.label.push_f64_2dp(pct);
            data.label.push_str("%]");
            data.used = used_mb;
            data.total = total_mb;
        }
    }
}

/// Collect memory info from /proc/meminfo.
pub fn collect_mem(data: &mut MemData) {
    let mut buf = [0u8; 4096];
    let n = raw_read(P_MEMINFO, &mut buf);
    let content = &buf[..n];

    let total = find_val(content, b"MemTotal", b':').map_or(0, parse_u64_b);
    let avail = find_val(content, b"MemAvailable", b':').map_or(0, parse_u64_b);
    let used = total.saturating_sub(avail);

    data.total_kb = total;
    data.used_kb = used;
    data.percent = if total > 0 {
        (used as f64 / total as f64) * 100.0
    } else {
        0.0
    };
    data.used_gb.push_f64_2dp(used as f64 / (1024.0 * 1024.0));
    data.total_gb.push_f64_2dp(total as f64 / (1024.0 * 1024.0));
}

/// Parse systemd session files in /run/systemd/sessions/ to extract login
/// time and client IP without spawning a subprocess.
fn try_systemd_sessions(user_bytes: &[u8], data: &mut LoginData, client_ip: &mut Buf<64>) {
    unsafe {
        let dir = libc::opendir(P_SD_SESSIONS as *const libc::c_char);
        if dir.is_null() {
            return;
        }

        let mut latest_usec: u64 = 0;
        let mut best_time: Buf<64> = Buf::new();
        let mut best_host: Buf<64> = Buf::new();
        let mut best_display: Buf<64> = Buf::new();

        loop {
            let entry = libc::readdir(dir);
            if entry.is_null() {
                break;
            }
            let name = std::ffi::CStr::from_ptr((*entry).d_name.as_ptr());
            let name_bytes = name.to_bytes();
            if name_bytes.starts_with(b".") || name_bytes.ends_with(b".ref") {
                continue;
            }

            // Build path: /run/systemd/sessions/<name>\0
            let mut path = [0u8; 128];
            let prefix = b"/run/systemd/sessions/";
            let plen = prefix.len();
            path[..plen].copy_from_slice(prefix);
            let nlen = name_bytes.len().min(128 - plen - 1);
            path[plen..plen + nlen].copy_from_slice(&name_bytes[..nlen]);

            let mut fbuf = [0u8; 1024];
            let flen = raw_read(path.as_ptr(), &mut fbuf);
            if flen == 0 {
                continue;
            }
            let content = &fbuf[..flen];

            // Only consider sessions for our user with CLASS=user
            let sess_user = find_val(content, b"USER", b'=');
            if sess_user.map_or(true, |u| u != user_bytes) {
                continue;
            }
            let class = find_val(content, b"CLASS", b'=');
            if class.map_or(false, |c| c != b"user") {
                continue;
            }

            // REALTIME= is epoch microseconds
            let rt = find_val(content, b"REALTIME", b'=').map_or(0u64, parse_u64_b);
            if rt <= latest_usec {
                continue;
            }
            latest_usec = rt;

            best_time = Buf::new();
            format_timestamp((rt / 1_000_000) as i64, &mut best_time);
            best_host = Buf::new();
            best_display = Buf::new();
            if let Some(h) = find_val(content, b"REMOTE_HOST", b'=') {
                best_host.push_bytes(h);
            }
            if let Some(d) = find_val(content, b"DISPLAY", b'=') {
                best_display.push_bytes(d);
            }
        }
        libc::closedir(dir);

        if latest_usec > 0 {
            if data.time.is_empty() {
                data.time = best_time;
            }
            if client_ip.is_empty() {
                if !best_host.is_empty() {
                    *client_ip = best_host;
                    data.ip = *client_ip;
                    data.has_ip = true;
                } else if !best_display.is_empty() {
                    client_ip.push_bytes(best_display.as_bytes());
                    data.ip = *client_ip;
                    data.has_ip = true;
                }
            }
        }
    }
}

/// Collect login info from utmp (fallback: systemd sessions) and uptime.
fn collect_login(data: &mut LoginData, user_bytes: &[u8], client_ip: &mut Buf<64>) {
    // Try utmp first (/var/run/utmp or /run/utmp)
    let mut entries: [UtmpEntry; 64] = unsafe { std::mem::zeroed() };
    let count = {
        let c1 = raw_read_entries(P_UTMP1, &mut entries);
        if c1 > 0 {
            c1
        } else {
            raw_read_entries(P_UTMP2, &mut entries)
        }
    };

    let my_tty = get_current_tty();
    let my_tty_len = cstr_len(&my_tty);

    // Find client IP from the utmp entry matching our tty
    let mut found_client = false;
    for entry in &entries[..count] {
        if entry.ut_type != UT_USER_PROCESS {
            continue;
        }
        let ulen = cstr_len(&entry.ut_user);
        if ulen != user_bytes.len() || entry.ut_user[..ulen] != *user_bytes {
            continue;
        }
        let llen = cstr_len(&entry.ut_line);
        if my_tty_len > 0 && llen == my_tty_len && entry.ut_line[..llen] == my_tty[..my_tty_len] {
            let hlen = cstr_len(&entry.ut_host);
            if hlen > 0 {
                client_ip.push_bytes(&entry.ut_host[..hlen]);
            }
            found_client = true;
            break;
        }
    }
    if !found_client && client_ip.is_empty() {
        client_ip.push_str("Not connected");
    }

    // Find most recent login timestamp for this user
    let mut latest_ts: i32 = 0;
    let mut latest_idx: Option<usize> = None;
    for (i, entry) in entries[..count].iter().enumerate() {
        if entry.ut_type != UT_USER_PROCESS {
            continue;
        }
        let ulen = cstr_len(&entry.ut_user);
        if ulen != user_bytes.len() || entry.ut_user[..ulen] != *user_bytes {
            continue;
        }
        if entry.ut_tv_sec > latest_ts {
            latest_ts = entry.ut_tv_sec;
            latest_idx = Some(i);
        }
    }
    if let Some(idx) = latest_idx {
        let entry = &entries[idx];
        format_timestamp(entry.ut_tv_sec as i64, &mut data.time);
        let hlen = cstr_len(&entry.ut_host);
        if hlen > 0 {
            data.ip.push_bytes(&entry.ut_host[..hlen]);
            data.has_ip = true;
        }
    }

    // Fallback: parse systemd session files directly (no subprocess)
    if data.time.is_empty() || client_ip.is_empty() {
        try_systemd_sessions(user_bytes, data, client_ip);
    }
    if data.time.is_empty() {
        data.time.push_str("Unknown");
    }
    if client_ip.is_empty() {
        client_ip.push_str("Not connected");
    }

    // Uptime from /proc/uptime
    let mut ubuf = [0u8; 64];
    let un = raw_read(P_UPTIME, &mut ubuf);
    let secs = parse_f64_b(&ubuf[..un]) as u64;
    let days = secs / 86400;
    let hours = (secs % 86400) / 3600;
    let mins = (secs % 3600) / 60;
    if days > 0 {
        data.uptime.push_u64(days);
        data.uptime.push_str("d ");
    }
    if hours > 0 || days > 0 {
        data.uptime.push_u64(hours);
        data.uptime.push_str("h ");
    }
    data.uptime.push_u64(mins);
    data.uptime.push_byte(b'm');
}

/// Collect all system data (one-shot, called once at startup).
pub fn collect(info: &mut SysInfo) {
    let mut user_buf = [0u8; 64];
    let user_len;
    unsafe {
        let u = libc::getenv(b"USER\0".as_ptr() as *const libc::c_char);
        if !u.is_null() {
            let s = std::ffi::CStr::from_ptr(u).to_bytes();
            let n = s.len().min(63);
            user_buf[..n].copy_from_slice(&s[..n]);
            user_len = n;
        } else {
            user_buf[..7].copy_from_slice(b"unknown");
            user_len = 7;
        }
    }
    let user_bytes = &user_buf[..user_len];

    collect_os(&mut info.os);
    collect_net(&mut info.net, user_bytes);
    collect_cpu(&mut info.cpu);
    collect_disk(&mut info.disk);
    collect_mem(&mut info.mem);
    collect_login(&mut info.login, user_bytes, &mut info.net.client_ip);
}
