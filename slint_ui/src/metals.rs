// Copyright (c) 2025 vivo Mobile Communication Co., Ltd.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//       http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

// Gold & silver price fetcher for the launcher MetalsPage.
// Mirrors apps/example/metals_clock: Tencent COMEX quote (1 request for both
// metals) + bilibili unix-timestamp time sync, displayed via a locally-derived
// wall clock so the per-second tick costs zero network requests.
//
// A slint::Timer drives the state machine on the UI thread. HTTP requests are
// blocking, bounded by SO_RCVTIMEO, and only run while this page is active.

use crate::app_window::MainWindow;
use crate::{syscall_error, uptime_millis};
use librs::syscall::Syscall;
use slint::ComponentHandle;
use std::cell::RefCell;
use std::io::{Error, ErrorKind, Result as IoResult};
use std::rc::Rc;

// Board reaches the public APIs through a plain-HTTP reverse proxy on the host
// (kernel has TCP but no DNS/TLS). See AGENTS.md "联网 Slint app".
const HTTP_HOST_IP: [u8; 4] = [10, 171, 198, 12];
const TX_PORT: u16 = 18085;
const TX_HOST: &str = "qt.gtimg.cn";
const TX_PATH: &str = "/q=hf_GC,hf_SI";
const TIME_PORT: u16 = 18086;
const TIME_PATH: &str = "/xlive/open-interface/v1/rtc/getTimestamp";
const TIME_HOST: &str = "api.live.bilibili.com";

const TICK_MS: u64 = 1000; // per-second wall-clock tick
const REFRESH_MS: u128 = 60 * 1000; // price refresh cadence
const TIME_RESYNC_MS: u128 = 10 * 60 * 1000; // time drift re-sync cadence
const FIRST_FETCH_DELAY_MS: u128 = 1500; // let the network settle after boot

const MAX_BODY: usize = 4 * 1024;
// 8 KiB reserve was OOMing the kernel heap (8704-byte alloc) after Wi-Fi
// init; the proxy's header is ~300 bytes so 1 KiB is ample.
const MAX_HEAD: usize = 1 * 1024;
const READ_CHUNK: usize = 512;

// ---------------------------------------------------------------------------
// Wall clock: network-synced unix seconds + local monotonic derivation.
// ---------------------------------------------------------------------------

struct Clock {
    server_ts: u64,
    mono_at_sync: u128,
}

impl Clock {
    const fn new() -> Self {
        Self {
            server_ts: 0,
            mono_at_sync: 0,
        }
    }
    fn sync(&mut self, ts: u64) {
        self.server_ts = ts;
        self.mono_at_sync = uptime_millis();
    }
    /// Current Beijing time as "HH:MM" (UTC+8, no DST — integer math is exact).
    fn now_hhmm(&self) -> Option<std::string::String> {
        if self.server_ts == 0 {
            return None;
        }
        let elapsed_ms = uptime_millis().saturating_sub(self.mono_at_sync);
        let now = self.server_ts + (elapsed_ms / 1000) as u64;
        let day_secs = (now + 8 * 3600) % 86_400;
        Some(format!(
            "{:02}:{:02}",
            day_secs / 3600,
            (day_secs % 3600) / 60
        ))
    }
}

// ---------------------------------------------------------------------------
// Minimal blocking HTTP client over librs sockets.
// ---------------------------------------------------------------------------

struct TcpSocket {
    fd: libc::c_int,
}

impl TcpSocket {
    fn connect(ip: [u8; 4], port: u16) -> IoResult<Self> {
        println!(
            "HTTP CONNECT {}.{}.{}.{}:{}",
            ip[0], ip[1], ip[2], ip[3], port
        );
        let fd = librs::net::socket::socket(libc::AF_INET, libc::SOCK_STREAM, 0);
        if fd < 0 {
            return Err(syscall_error(fd));
        }
        // Don't block the UI thread forever on a dead network.
        let tv = libc::timeval {
            tv_sec: 10,
            tv_usec: 0,
        };
        unsafe {
            let ret = librs::syscall::sys::Sys::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                &tv as *const libc::timeval as *const libc::c_void,
                core::mem::size_of::<libc::timeval>() as libc::socklen_t,
            );
            if let Err(librs::errno::Errno(errno)) = ret {
                let _ = librs::syscall::sys::Sys::close(fd);
                return Err(Error::from_raw_os_error(errno));
            }
        }
        let addr = libc::sockaddr_in {
            sin_len: core::mem::size_of::<libc::sockaddr_in>() as u8,
            sin_family: libc::AF_INET as libc::sa_family_t,
            sin_port: port.to_be(),
            sin_addr: libc::in_addr {
                s_addr: u32::from_ne_bytes(ip),
            },
            sin_vport: 0,
            sin_zero: [0; 6],
        };
        let ret = unsafe {
            librs::syscall::sys::Sys::connect(
                fd,
                &addr as *const libc::sockaddr_in as *const libc::sockaddr,
                core::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            )
        };
        if let Err(librs::errno::Errno(errno)) = ret {
            println!("HTTP CONNECT ERR errno={}", errno);
            let _ = librs::syscall::sys::Sys::close(fd);
            return Err(Error::from_raw_os_error(errno));
        }
        println!("HTTP CONNECT OK port={}", port);
        Ok(Self { fd })
    }
    fn write_all(&mut self, mut buf: &[u8]) -> IoResult<()> {
        while !buf.is_empty() {
            match librs::syscall::sys::Sys::send(self.fd, buf, 0) {
                Ok(0) => return Err(Error::new(ErrorKind::WriteZero, "socket write zero")),
                Ok(n) => buf = &buf[n..],
                Err(librs::errno::Errno(errno)) => return Err(Error::from_raw_os_error(errno)),
            }
        }
        Ok(())
    }
    fn read(&mut self, buf: &mut [u8]) -> IoResult<usize> {
        match librs::syscall::sys::Sys::recv(self.fd, buf, 0) {
            Ok(n) => Ok(n),
            Err(librs::errno::Errno(errno)) => Err(Error::from_raw_os_error(errno)),
        }
    }
}

impl Drop for TcpSocket {
    fn drop(&mut self) {
        let _ = librs::syscall::sys::Sys::close(self.fd);
    }
}

fn http_get(ip: [u8; 4], port: u16, path: &str, host: &str) -> IoResult<(u16, Vec<u8>)> {
    let mut sock = TcpSocket::connect(ip, port)?;
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: slint_ui/1.0\r\nAccept: */*\r\nConnection: close\r\n\r\n",
        path, host
    );
    sock.write_all(request.as_bytes())?;

    let mut raw: Vec<u8> = Vec::with_capacity(MAX_HEAD + READ_CHUNK);
    let mut chunk = [0u8; READ_CHUNK];
    let raw_cap = MAX_HEAD + MAX_BODY;
    loop {
        if raw.len() >= raw_cap {
            break;
        }
        let n = sock.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&chunk[..n]);
    }

    let sep = find_subslice(&raw, b"\r\n\r\n")
        .ok_or_else(|| Error::new(ErrorKind::InvalidData, "no HTTP head"))?;
    let head = &raw[..sep];
    let mut body = raw[sep + 4..].to_vec();
    let head_str = std::string::String::from_utf8_lossy(head);
    let status = head_str
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse::<u16>().ok())
        .ok_or_else(|| Error::new(ErrorKind::InvalidData, "bad status line"))?;
    let body_bytes = if head_str
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        dechunk(&body, MAX_BODY)?
    } else {
        body.truncate(MAX_BODY);
        body
    };
    Ok((status, body_bytes))
}

fn dechunk(data: &[u8], cap: usize) -> IoResult<Vec<u8>> {
    let mut out = Vec::with_capacity(READ_CHUNK);
    let mut pos = 0usize;
    loop {
        let mut line_end = None;
        let mut i = pos;
        while i + 1 < data.len() {
            if data[i] == b'\r' && data[i + 1] == b'\n' {
                line_end = Some(i);
                break;
            }
            i += 1;
        }
        let line_end = line_end
            .ok_or_else(|| Error::new(ErrorKind::UnexpectedEof, "chunk size line truncated"))?;
        let size_str = std::string::String::from_utf8_lossy(&data[pos..line_end]);
        let chunk_size = usize::from_str_radix(size_str.trim(), 16)
            .map_err(|_| Error::new(ErrorKind::InvalidData, "bad chunk size"))?;
        pos = line_end + 2;
        if chunk_size == 0 {
            break;
        }
        if pos + chunk_size > data.len() {
            let have = data.len().saturating_sub(pos);
            out.extend_from_slice(&data[pos..pos + have]);
            break;
        }
        out.extend_from_slice(&data[pos..pos + chunk_size]);
        pos += chunk_size;
        if pos + 1 < data.len() && data[pos] == b'\r' && data[pos + 1] == b'\n' {
            pos += 2;
        }
        if out.len() >= cap || pos >= data.len() {
            break;
        }
    }
    out.truncate(cap);
    Ok(out)
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    (0..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
}

// ---------------------------------------------------------------------------
// Data extraction — byte scans only; no serde, no regex.
// ---------------------------------------------------------------------------

/// Parse Tencent quote text for one instrument line. Field 0 is current price.
///   v_hf_GC="4466.56,-0.22,...";
fn extract_tencent_price(body: &[u8], key: &str) -> Option<std::string::String> {
    let mut needle = std::string::String::from("v_");
    needle.push_str(key);
    needle.push_str("=\"");
    let pos = find_subslice(body, needle.as_bytes())?;
    let rest = &body[pos + needle.len()..];
    let end = find_subslice(rest, b",")?;
    if end == 0 {
        return None;
    }
    let raw = core::str::from_utf8(&rest[..end]).ok()?;
    if !raw.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
        return None;
    }
    Some(raw.to_string())
}

/// Extract `"timestamp":1788828302` from bilibili JSON (u64 unix seconds).
fn extract_timestamp(json: &[u8]) -> Option<u64> {
    let pos = find_subslice(json, b"\"timestamp\":")?;
    let mut i = pos + b"\"timestamp\":".len();
    while i < json.len() && (json[i] == b' ' || json[i] == b'\t') {
        i += 1;
    }
    let start = i;
    while i < json.len() && json[i].is_ascii_digit() {
        i += 1;
    }
    if i == start {
        return None;
    }
    core::str::from_utf8(&json[start..i]).ok()?.parse().ok()
}

// ---------------------------------------------------------------------------
// WiFi station bring-up. The built-in (non-OTA) firmware does not connect on
// its own, so the page connects to the hotspot before sending TCP traffic.
// ---------------------------------------------------------------------------

const WIFI_SSID: &str = "bluekernel1";
const WIFI_PASSPHRASE: &str = "12345678";
const WIFI_CONNECT_GRACE_MS: u128 = 3_000; // association grace period after connect ioctl

#[derive(Clone, Copy, PartialEq)]
enum WifiState {
    Idle,
    Connecting { started_at: u128 },
    Connected,
    Failed,
}

fn wlan0_name() -> [libc::c_char; 16] {
    let mut name = [0 as libc::c_char; 16];
    for (dst, src) in name.iter_mut().zip(b"wlan0\0") {
        *dst = *src as libc::c_char;
    }
    name
}

fn open_ctl_socket() -> IoResult<libc::c_int> {
    let fd = librs::net::socket::socket(libc::AF_INET, libc::SOCK_DGRAM, 0);
    if fd < 0 {
        Err(syscall_error(fd))
    } else {
        Ok(fd)
    }
}

fn set_passphrase(fd: libc::c_int, passphrase: &str) -> IoResult<()> {
    let point = libc::iw_point {
        pointer: passphrase.as_ptr() as *mut libc::c_void,
        length: passphrase.len() as u16,
        flags: 0,
    };
    let mut req = libc::iwreq {
        ifr_ifrn: libc::__c_anonymous_iwreq {
            ifrn_name: wlan0_name(),
        },
        u: libc::iwreq_data { encoding: point },
    };
    match unsafe {
        librs::syscall::sys::Sys::ioctl(
            fd,
            libc::SIOCSIWENCODE,
            &mut req as *mut libc::iwreq as *mut libc::c_void,
        )
    } {
        Ok(ret) if ret < 0 => Err(syscall_error(ret)),
        Ok(_) => Ok(()),
        Err(librs::errno::Errno(errno)) => Err(Error::from_raw_os_error(errno)),
    }
}

fn trigger_connect(fd: libc::c_int, ssid: &str) -> IoResult<()> {
    let point = libc::iw_point {
        pointer: ssid.as_ptr() as *mut libc::c_void,
        length: ssid.len() as u16,
        flags: 0,
    };
    let mut req = libc::iwreq {
        ifr_ifrn: libc::__c_anonymous_iwreq {
            ifrn_name: wlan0_name(),
        },
        u: libc::iwreq_data { essid: point },
    };
    match unsafe {
        librs::syscall::sys::Sys::ioctl(
            fd,
            libc::SIOCSIWESSID,
            &mut req as *mut libc::iwreq as *mut libc::c_void,
        )
    } {
        Ok(ret) if ret < 0 => Err(syscall_error(ret)),
        Ok(_) => Ok(()),
        Err(librs::errno::Errno(errno)) => Err(Error::from_raw_os_error(errno)),
    }
}

// Fetcher state machine, driven by a per-second slint::Timer.
// ---------------------------------------------------------------------------

struct MetalsFetcher {
    clock: Clock,
    started_at: u128,
    last_refresh_ms: u128,
    last_resync_ms: u128,
    last_minute: Option<std::string::String>,
    refresh_requested: bool,
    active: bool, // only fetch while the Metals page is on screen
    wifi_state: WifiState,
    ctl_socket: Option<libc::c_int>,
}

impl MetalsFetcher {
    fn new() -> Self {
        let now = uptime_millis();
        Self {
            clock: Clock::new(),
            started_at: now,
            last_refresh_ms: 0,
            last_resync_ms: 0,
            last_minute: None,
            // Trigger the first fetch after the page becomes active.
            refresh_requested: true,
            active: false,
            wifi_state: WifiState::Idle,
            ctl_socket: None,
        }
    }

    fn request_refresh(&mut self) {
        self.refresh_requested = true;
    }

    fn set_active(&mut self, active: bool) {
        // Entering the page triggers an immediate refresh + time re-sync.
        if active && !self.active {
            self.refresh_requested = true;
            self.last_resync_ms = 0;
            if self.wifi_state == WifiState::Failed {
                self.wifi_state = WifiState::Idle;
            }
        }
        self.active = active;
    }

    /// Drive the WiFi state machine; returns true once the link is up.
    fn ensure_wifi(&mut self, ui: &MainWindow, now: u128) -> bool {
        match self.wifi_state {
            WifiState::Connected => true,
            WifiState::Failed => false,
            WifiState::Idle => {
                ui.set_metals_status("Connecting WiFi...".into());
                match open_ctl_socket() {
                    Ok(fd) => self.ctl_socket = Some(fd),
                    Err(_) => {
                        println!("WIFI SOCK ERR");
                        self.wifi_state = WifiState::Failed;
                        ui.set_metals_status("WiFi socket failed".into());
                        return false;
                    }
                }
                let fd = self.ctl_socket.unwrap();
                if let Err(_) = set_passphrase(fd, WIFI_PASSPHRASE) {
                    println!("WIFI PSK ERR");
                }
                match trigger_connect(fd, WIFI_SSID) {
                    Ok(()) => {
                        println!("WIFI CONNECTING");
                        self.wifi_state = WifiState::Connecting { started_at: now };
                    }
                    Err(_) => {
                        println!("WIFI CONN ERR");
                        self.wifi_state = WifiState::Failed;
                        ui.set_metals_status("WiFi connect failed".into());
                    }
                }
                false
            }
            WifiState::Connecting { started_at } => {
                // The kernel's SIOCGIFFLAGS path does not write the flags back
                // to userspace, so we can't poll for a real link-up event.
                // Wait briefly for station association after the connect ioctl.
                if now.saturating_sub(started_at) >= WIFI_CONNECT_GRACE_MS {
                    println!("WIFI UP");
                    self.wifi_state = WifiState::Connected;
                    ui.set_metals_status("WiFi connected".into());
                    true
                } else {
                    false
                }
            }
        }
    }

    /// One Tencent request updates both prices. Static-string logs only.
    fn fetch_prices(&mut self, ui: &MainWindow) {
        match http_get(HTTP_HOST_IP, TX_PORT, TX_PATH, TX_HOST) {
            Ok((200, body)) => {
                println!("TX 200");
                if let Some(gold) = extract_tencent_price(&body, "hf_GC") {
                    ui.set_metals_xau(gold.into());
                }
                if let Some(silver) = extract_tencent_price(&body, "hf_SI") {
                    ui.set_metals_xag(silver.into());
                }
                ui.set_metals_status("Updated".into());
            }
            Ok((code, _)) => {
                println!("TX !200");
                ui.set_metals_status(format!("HTTP {}", code).into());
            }
            Err(err) => {
                println!("TX ERR kind={:?} raw={:?}", err.kind(), err.raw_os_error());
                ui.set_metals_status("Price fetch failed".into());
            }
        }
    }

    fn sync_time(&mut self) {
        match http_get(HTTP_HOST_IP, TIME_PORT, TIME_PATH, TIME_HOST) {
            Ok((200, body)) => {
                if let Some(ts) = extract_timestamp(&body) {
                    self.clock.sync(ts);
                    println!("TIME OK");
                } else {
                    println!("TIME PARSE");
                }
            }
            Ok((_, _)) => println!("TIME !200"),
            Err(err) => println!(
                "TIME ERR kind={:?} raw={:?}",
                err.kind(),
                err.raw_os_error()
            ),
        }
    }

    fn tick(&mut self, ui: &MainWindow) {
        let now = uptime_millis();

        // Per-second wall-clock tick, but only rewrite the property on a
        // minute change (the equality guard avoids per-frame allocation).
        if let Some(hm) = self.clock.now_hhmm() {
            if self.last_minute.as_deref() != Some(hm.as_str()) {
                ui.set_metals_time(hm.clone().into());
                self.last_minute = Some(hm);
            }
        }

        if !self.active {
            return;
        }

        // Wait for WiFi association before sending TCP traffic.
        if !self.ensure_wifi(ui, now) {
            return;
        }

        let first_due = now.saturating_sub(self.started_at) >= FIRST_FETCH_DELAY_MS;
        let refresh_due = self.refresh_requested && first_due
            || (first_due && now.saturating_sub(self.last_refresh_ms) >= REFRESH_MS);
        if refresh_due {
            self.refresh_requested = false;
            self.last_refresh_ms = now;
            println!("REFRESH");
            self.fetch_prices(ui);
        }

        // Sync immediately once Wi-Fi is up (last_resync_ms == 0 means
        // "never synced"); then re-sync every TIME_RESYNC_MS.
        let sync_due =
            self.last_resync_ms == 0 || now.saturating_sub(self.last_resync_ms) >= TIME_RESYNC_MS;
        if first_due && sync_due {
            self.last_resync_ms = now;
            self.sync_time();
        }
    }
}

/// Connect the metals fetcher to the shared launcher window. The returned
/// timer must stay alive for as long as the Slint event loop runs.
pub(crate) fn install(ui: &MainWindow) -> slint::Timer {
    let fetcher = Rc::new(RefCell::new(MetalsFetcher::new()));

    let cb_fetcher = fetcher.clone();
    ui.on_metals_refresh(move || {
        cb_fetcher.borrow_mut().request_refresh();
    });

    let active_fetcher = fetcher.clone();
    ui.on_metals_active_changed(move |active| {
        active_fetcher.borrow_mut().set_active(active);
    });

    let timer = slint::Timer::default();
    let timer_ui = ui.as_weak();
    timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(TICK_MS),
        move || {
            if let Some(ui) = timer_ui.upgrade() {
                fetcher.borrow_mut().tick(&ui);
            }
        },
    );
    timer
}
