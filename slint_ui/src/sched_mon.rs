// Copyright (c) 2026 vivo Mobile Communication Co., Ltd.
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

// Scheduler and resource monitor backend.
// Polls /proc/stat, /proc/meminfo, and /proc/<tid>/status on the UI thread
// via a repeating slint::Timer, following the imu.rs pattern.

use crate::app_window::MainWindow;
use crate::syscall_error;
use librs::c_str::CStr;
use librs::syscall::Syscall;
use slint::ComponentHandle;
use std::cell::RefCell;
use std::io::{Error, ErrorKind, Result as IoResult};
use std::rc::Rc;

const POLL_MS: u64 = 500; // 2 Hz refresh rate
const CORE_COUNT: usize = 1; // ESP32-C6 is single-core RISC-V
const MAX_TASK_LINES: usize = 4;

/// Snapshot of CPU idle/system ticks for computing delta usage.
#[derive(Clone, Copy, Default)]
struct CpuTickSnapshot {
    idle: u64,
    system: u64,
}

/// Parse a single "cpuN  ..." line from /proc/stat.
/// Returns (cpu_id, idle_ticks, total_ticks) or None if not a cpu line.
fn parse_cpu_stat_line(line: &str) -> Option<(usize, u64, u64)> {
    let line = line.trim();
    if !line.starts_with("cpu") {
        return None;
    }
    let rest = line.strip_prefix("cpu")?;
    let (id_str, values) = rest.split_once(' ')?;
    let cpu_id = id_str.parse::<usize>().ok()?;
    let parts: Vec<u64> = values
        .split_whitespace()
        .filter_map(|s| s.parse::<u64>().ok())
        .collect();
    if parts.len() < 5 {
        return None;
    }
    // user, nice, system, idle, iowait, irq, softirq, ...
    let user = parts[0];
    let nice = parts[1];
    let system = parts[2];
    let idle = parts[3];
    let total = user + nice + system + idle + parts.iter().skip(4).sum::<u64>();
    Some((cpu_id, idle, total))
}

/// Parse /proc/stat content and return per-core idle/total tick snapshots.
fn parse_proc_stat(content: &[u8]) -> Vec<CpuTickSnapshot> {
    let text = core::str::from_utf8(content).unwrap_or("");
    let mut snaps: Vec<CpuTickSnapshot> = Vec::with_capacity(CORE_COUNT);
    for line in text.lines() {
        if let Some((cpu_id, idle, total)) = parse_cpu_stat_line(line) {
            if cpu_id < CORE_COUNT {
                snaps.push(CpuTickSnapshot {
                    idle,
                    ..Default::default()
                });
                // Store total in the system field (reuse field)
                if let Some(entry) = snaps.last_mut() {
                    entry.system = total;
                }
            }
        }
    }
    snaps
}

/// Parse /proc/meminfo content. Returns (total_kb, used_kb, max_used_kb).
fn parse_proc_meminfo(content: &[u8]) -> (f32, f32, f32) {
    let text = core::str::from_utf8(content).unwrap_or("");
    let mut total = 0.0;
    let mut used = 0.0;
    let mut max_used = 0.0;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("MemTotal:") {
            total = parse_kb_value(line);
        } else if line.starts_with("MemUsed:") {
            used = parse_kb_value(line);
        } else if line.starts_with("MemMaxUsed:") {
            max_used = parse_kb_value(line);
        }
    }
    (total, used, max_used)
}

/// Extract the numeric kB value from a line like "MemTotal: 1234 kB".
fn parse_kb_value(line: &str) -> f32 {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() >= 2 {
        parts[1].parse::<f32>().unwrap_or(0.0)
    } else {
        0.0
    }
}

/// Parse a thread status file from /proc/<tid>/status.
/// Returns (tid_display, kind_abbr, state_abbr, prio_display, typed_name).
/// PROCFS status provides Name (=thread kind), State, and Priority.
/// The returned "typed_name" is the human-readable thread kind (e.g. "Idle Task").
fn parse_thread_status(content: &[u8], tid: usize) -> (String, String, String, String, String) {
    let text = core::str::from_utf8(content).unwrap_or("");
    let mut kind = "normal";
    let mut state = "unknown";
    let mut priority = 0usize;

    for line in text.lines() {
        let line = line.trim();
        if let Some(val) = line.strip_prefix("Name:") {
            kind = val.trim();
        } else if let Some(val) = line.strip_prefix("State:") {
            state = val.trim();
        } else if let Some(val) = line.strip_prefix("Priority:") {
            priority = val.trim().parse::<usize>().unwrap_or(0);
        }
    }

    // State abbreviations for compact display
    let state_abbr = match state {
        "running" => "RUN",
        "ready" => "RDY",
        "suspended" => "SUS",
        "idle" => "IDLE",
        "retired" => "FIN",
        _ => "?",
    };

    // Kind abbreviation for compact Type column
    let type_abbr = match kind {
        "idle" => "idle",
        "normal" => "norm",
        "async_poller" => "poll",
        "soft_timer" => "timer",
        _ => kind,
    };

    // Human-readable name derived from thread kind
    let typed_name = match kind {
        "idle" => "Idle Task",
        "normal" => "Main",
        "async_poller" => "Async Poller",
        "soft_timer" => "Soft Timer",
        _ => kind,
    };

    // TID: show last 4 hex digits
    let tid_str = format!("{:04X}", tid & 0xFFFF);
    let prio_str = format!("{}", priority);

    (tid_str, type_abbr.to_string(), state_abbr.to_string(), prio_str, typed_name.to_string())
}

/// Read the full content of a file (small, procfs-style).
fn read_proc_file(path: &[u8]) -> IoResult<Vec<u8>> {
    let c_path = CStr::from_bytes_with_nul(path)
        .map_err(|_| Error::from_raw_os_error(libc::EINVAL))?;
    let fd = librs::syscall::sys::Sys::open(c_path, libc::O_RDONLY, 0);
    if fd < 0 {
        return Err(syscall_error(fd));
    }

    let mut buf = [0u8; 1024];
    let n = match librs::syscall::sys::Sys::read(fd, &mut buf) {
        Ok(n) => n,
        Err(librs::errno::Errno(errno)) => {
            let _ = librs::syscall::sys::Sys::close(fd);
            return Err(Error::from_raw_os_error(errno));
        }
    };
    let _ = librs::syscall::sys::Sys::close(fd);
    Ok(buf[..n].to_vec())
}

/// List directory entries in /proc (each is a TID directory).
/// Uses the kernel's dirent layout (which matches libc::dirent64 on 32-bit musl).
fn list_proc_entries() -> IoResult<Vec<usize>> {
    let path = CStr::from_bytes_with_nul(b"/proc\0")
        .map_err(|_| Error::from_raw_os_error(libc::EINVAL))?;
    let fd = librs::syscall::sys::Sys::open(path, libc::O_RDONLY | libc::O_DIRECTORY, 0);
    if fd < 0 {
        return Err(syscall_error(fd));
    }

    // Read directory entries using getdents
    let mut buf = [0u8; 512];
    let mut tids = Vec::new();
    loop {
        let n = match librs::syscall::sys::Sys::getdents(fd, &mut buf) {
            Ok(n) => n,
            Err(librs::errno::Errno(errno)) => {
                let _ = librs::syscall::sys::Sys::close(fd);
                return Err(Error::from_raw_os_error(errno));
            }
        };
        if n == 0 {
            break;
        }

        // Kernel dirent layout (RISC-V 32-bit, ino_t=u16, off_t=i32, repr(C) with alignment):
        //   d_ino:     u16   @0   (2 bytes)
        //   [padding]        @2   (2 bytes, align d_off to 4)
        //   d_off:     i32   @4   (4 bytes)
        //   d_reclen:  u16   @8   (2 bytes)
        //   d_type:    u8    @10  (1 byte)
        //   [padding]        @11  (1 byte, align d_namlen to 2)
        //   d_namlen:  u16   @12  (2 bytes)
        //   d_name:    [i8]  @14  (NAME_OFFSET = 14)
        const NAME_OFFSET: usize = 14;

        let mut offset: usize = 0;
        while offset + NAME_OFFSET <= n {
            let reclen = u16::from_le_bytes(
                buf[offset + 8..offset + 10].try_into().unwrap(),
            ) as usize;
            if reclen < NAME_OFFSET + 1 || offset + reclen > n {
                break;
            }

            let d_type = buf[offset + 10];
            let namlen = u16::from_le_bytes(
                buf[offset + 12..offset + 14].try_into().unwrap(),
            ) as usize;

            let name_len = namlen.min(reclen.saturating_sub(NAME_OFFSET + 1));
            let name_bytes = &buf[offset + NAME_OFFSET..offset + NAME_OFFSET + name_len];

            // Skip "." and ".."
            if d_type == 4 && name_len > 0 && name_bytes[0] != b'.' {
                if let Ok(name) = core::str::from_utf8(name_bytes) {
                    if let Ok(tid) = name.parse::<usize>() {
                        tids.push(tid);
                    }
                }
            }

            offset += reclen;
        }
    }
    let _ = librs::syscall::sys::Sys::close(fd);

    // Sort by TID
    tids.sort_unstable();
    Ok(tids)
}

struct SchedMonitor {
    prev_ticks: Vec<CpuTickSnapshot>,
    first_stat: bool,
}

impl SchedMonitor {
    fn new() -> Self {
        Self {
            prev_ticks: vec![CpuTickSnapshot::default(); CORE_COUNT],
            first_stat: true,
        }
    }

    fn refresh_task_list(&mut self, ui: &MainWindow) {
        // TODO: 后接接入 /proc/<tid>/status 真实读取
        // 当前使用占位数据演示
        let tids = ["0001", "0002", "0003", "0004"];
        let types_ = ["idle", "norm", "poll", "timer"];
        let states = ["IDLE", "RUN", "RDY", "SUS"];
        let prios =  ["0",  "15", "10", "15"];
        let names =  ["Idle Task", "Main", "Async Poller", "Soft Timer"];
        let mut tid_col: Vec<slint::SharedString> = Vec::with_capacity(MAX_TASK_LINES);
        let mut type_col: Vec<slint::SharedString> = Vec::with_capacity(MAX_TASK_LINES);
        let mut state_col: Vec<slint::SharedString> = Vec::with_capacity(MAX_TASK_LINES);
        let mut prio_col: Vec<slint::SharedString> = Vec::with_capacity(MAX_TASK_LINES);
        let mut name_col: Vec<slint::SharedString> = Vec::with_capacity(MAX_TASK_LINES);
        for i in 0..MAX_TASK_LINES {
            tid_col.push(tids[i].into());
            type_col.push(types_[i].into());
            state_col.push(states[i].into());
            prio_col.push(prios[i].into());
            name_col.push(names[i].into());
        }
        ui.set_task_tids(slint::ModelRc::new(slint::VecModel::from(tid_col)));
        ui.set_task_types(slint::ModelRc::new(slint::VecModel::from(type_col)));
        ui.set_task_states(slint::ModelRc::new(slint::VecModel::from(state_col)));
        ui.set_task_prios(slint::ModelRc::new(slint::VecModel::from(prio_col)));
        ui.set_task_names(slint::ModelRc::new(slint::VecModel::from(name_col)));
    }

    fn tick(&mut self, ui: &MainWindow) {
        // Only poll when the sched-mon page (app 6) is active
        if ui.get_current_app() != 6 {
            return;
        }

        // ---- CPU usage ----
        if let Ok(stat_content) = read_proc_file(b"/proc/stat\0") {
            let current_ticks = parse_proc_stat(&stat_content);
            if !self.first_stat && current_ticks.len() == self.prev_ticks.len() {
                // Calculate deltas
                let mut cpu_pcts: Vec<f32> = Vec::with_capacity(CORE_COUNT);
                for i in 0..current_ticks.len() {
                    let d_total = current_ticks[i].system.saturating_sub(self.prev_ticks[i].system);
                    let d_idle = current_ticks[i].idle.saturating_sub(self.prev_ticks[i].idle);
                    if d_total > 0 {
                        let pct = (d_total - d_idle) as f32 / d_total as f32 * 100.0;
                        cpu_pcts.push(pct.min(100.0));
                    } else {
                        cpu_pcts.push(0.0);
                    }
                }
                // Pad to CORE_COUNT
                while cpu_pcts.len() < CORE_COUNT {
                    cpu_pcts.push(0.0);
                }
                let model = slint::ModelRc::new(slint::VecModel::from(cpu_pcts));
                ui.set_cpu_usage_percent(model);
                ui.set_cpu_cores(CORE_COUNT as i32);
            }
            self.prev_ticks = current_ticks;
            self.first_stat = false;
        }

        // ---- Memory usage ----
        if let Ok(mem_content) = read_proc_file(b"/proc/meminfo\0") {
            let (total, used, max_used) = parse_proc_meminfo(&mem_content);
            // Only update when the value changes (to avoid unnecessary redraws)
            let eps = 0.5;
            if (total - ui.get_mem_total_kb()).abs() > eps {
                ui.set_mem_total_kb(total);
            }
            if (used - ui.get_mem_used_kb()).abs() > eps {
                ui.set_mem_used_kb(used);
            }
            if (max_used - ui.get_mem_max_used_kb()).abs() > eps {
                ui.set_mem_max_used_kb(max_used);
            }
        }
    }
}

/// Connect the scheduler monitor to the shared launcher window. The returned
/// timer must stay alive for as long as the Slint event loop runs.
pub(crate) fn install(ui: &MainWindow) -> slint::Timer {
    let monitor = Rc::new(RefCell::new(SchedMonitor::new()));

    // Bind the refresh-tasks callback from the Slint UI.
    {
        let monitor = monitor.clone();
        let refresh_ui = ui.as_weak();
        ui.on_refresh_tasks(move || {
            if let Some(ui) = refresh_ui.upgrade() {
                let mut mon = monitor.borrow_mut();
                mon.refresh_task_list(&ui);
            }
        });
    }

    let timer = slint::Timer::default();
    let timer_ui = ui.as_weak();
    timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(POLL_MS),
        move || {
            if let Some(ui) = timer_ui.upgrade() {
                let mut mon = monitor.borrow_mut();
                mon.tick(&ui);
            }
        },
    );
    timer
}