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

use crate::{app_window::MainWindow, uptime_micros};
use librs::{c_str::CStr, syscall::Syscall};
use slint::ComponentHandle;
use std::{
    cell::RefCell,
    io::{Error, ErrorKind, Result},
    rc::Rc,
};

const DEVICE_PATH: &[u8] = b"/dev/esp32-flash0\0";
const ERASE_RANGE_IOCTL: libc::c_ulong = 0x40;
const IOCTL_ABI_VERSION: u32 = 1;
const SECTOR_SIZE: usize = 4096;
const SLOT_COUNT: usize = 2048;
const HEADER_SIZE: usize = 32;
const PAYLOAD_SIZE: usize = SECTOR_SIZE - HEADER_SIZE;
const RECORDS_PER_RUN: usize = 32;
const MAGIC: u32 = 0x4246_494f;
const FORMAT_VERSION: u32 = 1;
const COMMIT_MARKER: u32 = 0x434f_4d4d;
const COMMIT_OFFSET: usize = 24;

#[repr(C)]
struct EraseRangeRequest {
    version: u32,
    size: u32,
    flags: u32,
    region_offset: u32,
    length: u32,
}

struct FlashDevice(libc::c_int);

impl FlashDevice {
    fn open() -> Result<Self> {
        let path = CStr::from_bytes_with_nul(DEVICE_PATH)
            .map_err(|_| Error::from_raw_os_error(libc::EINVAL))?;
        let fd = librs::syscall::sys::Sys::open(path, libc::O_RDWR, 0);
        if fd < 0 {
            Err(Error::from_raw_os_error(-fd))
        } else {
            Ok(Self(fd))
        }
    }

    fn seek(&self, offset: usize) -> Result<()> {
        let result = librs::syscall::sys::Sys::lseek(self.0, offset as libc::off_t, libc::SEEK_SET);
        if result < 0 {
            Err(Error::from_raw_os_error(-result as i32))
        } else {
            Ok(())
        }
    }

    fn read_exact_at(&self, offset: usize, mut data: &mut [u8]) -> Result<()> {
        self.seek(offset)?;
        while !data.is_empty() {
            match librs::syscall::sys::Sys::read(self.0, data) {
                Ok(0) => return Err(Error::new(ErrorKind::UnexpectedEof, "short Flash read")),
                Ok(count) => data = &mut data[count..],
                Err(librs::errno::Errno(errno)) => return Err(Error::from_raw_os_error(errno)),
            }
        }
        Ok(())
    }

    fn write_all_at(&self, offset: usize, mut data: &[u8]) -> Result<()> {
        self.seek(offset)?;
        while !data.is_empty() {
            match librs::syscall::sys::Sys::write(self.0, data) {
                Ok(0) => return Err(Error::new(ErrorKind::WriteZero, "short Flash write")),
                Ok(count) => data = &data[count..],
                Err(librs::errno::Errno(errno)) => return Err(Error::from_raw_os_error(errno)),
            }
        }
        Ok(())
    }

    fn erase_slot(&self, slot: usize) -> Result<()> {
        let mut request = EraseRangeRequest {
            version: IOCTL_ABI_VERSION,
            size: core::mem::size_of::<EraseRangeRequest>() as u32,
            flags: 0,
            region_offset: (slot * SECTOR_SIZE) as u32,
            length: SECTOR_SIZE as u32,
        };
        unsafe {
            librs::syscall::sys::Sys::ioctl(
                self.0,
                ERASE_RANGE_IOCTL,
                &mut request as *mut _ as *mut libc::c_void,
            )
            .map(|_| ())
            .map_err(|librs::errno::Errno(errno)| Error::from_raw_os_error(errno))
        }
    }
}

impl Drop for FlashDevice {
    fn drop(&mut self) {
        let _ = librs::syscall::sys::Sys::close(self.0);
    }
}

struct FlashJournal {
    used: [bool; SLOT_COUNT],
    next_slot: usize,
    next_sequence: u32,
}

impl FlashJournal {
    fn scan(device: &FlashDevice) -> Result<Self> {
        let mut used = [false; SLOT_COUNT];
        let mut latest: Option<(usize, u32)> = None;
        for slot in 0..SLOT_COUNT {
            let mut header = [0xff; HEADER_SIZE];
            device.read_exact_at(slot * SECTOR_SIZE, &mut header)?;
            used[slot] = header.iter().any(|byte| *byte != 0xff);
            let magic = read_u32(&header, 0);
            let version = read_u32(&header, 4);
            let sequence = read_u32(&header, 8);
            let length = read_u32(&header, 12) as usize;
            let commit = read_u32(&header, COMMIT_OFFSET);
            if magic == MAGIC
                && version == FORMAT_VERSION
                && sequence != 0
                && length <= PAYLOAD_SIZE
                && commit == COMMIT_MARKER
                && latest
                    .map(|(_, current)| sequence.wrapping_sub(current) < 0x8000_0000)
                    .unwrap_or(true)
            {
                latest = Some((slot, sequence));
            }
        }

        let start = latest.map(|(slot, _)| (slot + 1) % SLOT_COUNT).unwrap_or(0);
        let next_slot = (0..SLOT_COUNT)
            .map(|step| (start + step) % SLOT_COUNT)
            .find(|slot| !used[*slot])
            .unwrap_or(start);
        let next_sequence = latest
            .map(|(_, sequence)| sequence.wrapping_add(1).max(1))
            .unwrap_or(1);
        println!(
            "[FLASH_IO] scan complete used_slots={} next_slot={} next_sequence={}",
            used.iter().filter(|is_used| **is_used).count(),
            next_slot,
            next_sequence
        );
        Ok(Self {
            used,
            next_slot,
            next_sequence,
        })
    }

    fn write_next(&mut self, device: &FlashDevice) -> Result<(usize, u32, u128)> {
        let slot = self.next_slot;
        let sequence = self.next_sequence;
        let started_at = uptime_micros();
        let base = slot * SECTOR_SIZE;
        println!(
            "[FLASH_IO] begin slot={} sequence={} offset=0x{:08x} previously_used={}",
            slot, sequence, base, self.used[slot]
        );

        device.erase_slot(slot).map_err(|error| {
            Error::new(
                error.kind(),
                format!("erase failed: slot={slot} offset=0x{base:08x}: {error}"),
            )
        })?;

        let mut erased = vec![0u8; SECTOR_SIZE];
        device.read_exact_at(base, &mut erased).map_err(|error| {
            Error::new(
                error.kind(),
                format!("erase readback failed: slot={slot} offset=0x{base:08x}: {error}"),
            )
        })?;
        if let Some((index, actual)) = erased
            .iter()
            .copied()
            .enumerate()
            .find(|(_, byte)| *byte != 0xff)
        {
            return Err(Error::new(
                ErrorKind::InvalidData,
                format!(
                    "erase verify failed: slot={slot} offset=0x{:08x} expected=ff actual={actual:02x}",
                    base + index
                ),
            ));
        }
        println!("[FLASH_IO] erase verified slot={slot}");

        let mut payload = vec![0u8; PAYLOAD_SIZE];
        fill_payload(&mut payload, sequence);
        let crc = crc32(&payload);
        let mut header = [0xff; HEADER_SIZE];
        write_u32(&mut header, 0, MAGIC);
        write_u32(&mut header, 4, FORMAT_VERSION);
        write_u32(&mut header, 8, sequence);
        write_u32(&mut header, 12, PAYLOAD_SIZE as u32);
        write_u32(&mut header, 16, crc);

        device
            .write_all_at(base, &header[..COMMIT_OFFSET])
            .map_err(|error| {
                Error::new(
                    error.kind(),
                    format!("header write failed: slot={slot}: {error}"),
                )
            })?;
        device
            .write_all_at(base + HEADER_SIZE, &payload)
            .map_err(|error| {
                Error::new(
                    error.kind(),
                    format!("payload write failed: slot={slot}: {error}"),
                )
            })?;

        let mut verify = vec![0u8; PAYLOAD_SIZE];
        device
            .read_exact_at(base + HEADER_SIZE, &mut verify)
            .map_err(|error| {
                Error::new(
                    error.kind(),
                    format!("payload readback failed: slot={slot}: {error}"),
                )
            })?;
        let actual_crc = crc32(&verify);
        if let Some((index, (expected, actual))) = payload
            .iter()
            .copied()
            .zip(verify.iter().copied())
            .enumerate()
            .find(|(_, (expected, actual))| expected != actual)
        {
            return Err(Error::new(
                ErrorKind::InvalidData,
                format!(
                    "payload verify failed: slot={slot} offset=0x{:08x} expected={expected:02x} actual={actual:02x} crc={crc:08x}/{actual_crc:08x}",
                    base + HEADER_SIZE + index
                ),
            ));
        }
        if actual_crc != crc {
            return Err(Error::new(
                ErrorKind::InvalidData,
                format!(
                    "payload CRC failed: slot={slot} expected={crc:08x} actual={actual_crc:08x}"
                ),
            ));
        }

        device.write_all_at(base + COMMIT_OFFSET, &COMMIT_MARKER.to_le_bytes())?;
        let mut committed = [0u8; 4];
        device.read_exact_at(base + COMMIT_OFFSET, &mut committed)?;
        if u32::from_le_bytes(committed) != COMMIT_MARKER {
            return Err(Error::new(
                ErrorKind::InvalidData,
                format!(
                    "commit verify failed: slot={slot} expected={COMMIT_MARKER:08x} actual={:08x}",
                    u32::from_le_bytes(committed)
                ),
            ));
        }

        println!(
            "[FLASH_IO] verified slot={} sequence={} crc=0x{:08x} elapsed_us={}",
            slot,
            sequence,
            crc,
            uptime_micros().saturating_sub(started_at)
        );

        self.used[slot] = true;
        let search_start = (slot + 1) % SLOT_COUNT;
        self.next_slot = (0..SLOT_COUNT)
            .map(|step| (search_start + step) % SLOT_COUNT)
            .find(|candidate| !self.used[*candidate])
            .unwrap_or(search_start);
        self.next_sequence = sequence.wrapping_add(1).max(1);
        Ok((slot, sequence, uptime_micros().saturating_sub(started_at)))
    }
}

struct Controller {
    device: Option<FlashDevice>,
    journal: Option<FlashJournal>,
    remaining: usize,
    completed: usize,
    total_micros: u128,
}

impl Controller {
    fn new() -> Self {
        Self {
            device: None,
            journal: None,
            remaining: 0,
            completed: 0,
            total_micros: 0,
        }
    }

    fn start(&mut self, ui: &MainWindow) -> Result<()> {
        if self.device.is_none() {
            println!("[FLASH_IO] opening /dev/esp32-flash0");
            let device = FlashDevice::open()?;
            let journal = FlashJournal::scan(&device)?;
            self.device = Some(device);
            self.journal = Some(journal);
        }
        self.remaining = RECORDS_PER_RUN;
        self.completed = 0;
        self.total_micros = 0;
        ui.set_flash_bytes_written(0);
        ui.set_flash_speed_kbps(0);
        ui.set_flash_average_kbps(0);
        ui.set_flash_running(true);
        ui.set_flash_status_text("正在写入 Flash 槽位".into());
        Ok(())
    }

    fn tick(&mut self, ui: &MainWindow) {
        if self.remaining == 0 {
            return;
        }
        let result = self
            .journal
            .as_mut()
            .unwrap()
            .write_next(self.device.as_ref().unwrap());
        match result {
            Ok((slot, sequence, elapsed)) => {
                self.remaining -= 1;
                self.completed += 1;
                self.total_micros = self.total_micros.saturating_add(elapsed);
                let live = throughput_kib(elapsed, 1);
                let average = throughput_kib(self.total_micros, self.completed);
                ui.set_flash_slot(slot as i32);
                ui.set_flash_sequence(sequence as i32);
                ui.set_flash_bytes_written((self.completed * 4) as i32);
                ui.set_flash_speed_kbps(live);
                ui.set_flash_average_kbps(average);
                if self.remaining == 0 {
                    ui.set_flash_running(false);
                    ui.set_flash_status_text("写入并校验通过".into());
                }
            }
            Err(error) => {
                println!("[FLASH_IO] failed: {error}");
                self.remaining = 0;
                ui.set_flash_running(false);
                ui.set_flash_status_text(format!("Flash 错误: {error}").into());
            }
        }
    }
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn write_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn fill_payload(payload: &mut [u8], sequence: u32) {
    let mut value = sequence ^ 0xa5a5_5a5a;
    for byte in payload {
        value ^= value << 13;
        value ^= value >> 17;
        value ^= value << 5;
        *byte = value as u8;
    }
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in bytes {
        crc ^= *byte as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & (0u32.wrapping_sub(crc & 1)));
        }
    }
    !crc
}

fn throughput_kib(micros: u128, records: usize) -> i32 {
    if micros == 0 {
        return 0;
    }
    ((records as u128 * SECTOR_SIZE as u128 * 1_000_000) / (micros * 1024)).min(i32::MAX as u128)
        as i32
}

pub(crate) fn install(ui: &MainWindow) -> slint::Timer {
    let controller = Rc::new(RefCell::new(Controller::new()));
    let callback_controller = controller.clone();
    let ui_weak = ui.as_weak();
    ui.on_flash_start_requested(move || {
        if let Some(ui) = ui_weak.upgrade() {
            if let Err(error) = callback_controller.borrow_mut().start(&ui) {
                ui.set_flash_running(false);
                ui.set_flash_status_text(format!("Flash 不可用: {error}").into());
            }
        }
    });

    let timer = slint::Timer::default();
    let ui_weak = ui.as_weak();
    timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(30),
        move || {
            if let Some(ui) = ui_weak.upgrade() {
                controller.borrow_mut().tick(&ui);
            }
        },
    );
    timer
}
