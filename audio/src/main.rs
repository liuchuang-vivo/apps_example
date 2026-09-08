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

//! BlueOS Audio Example
//!
//! 1. Triggers a GDMA memory-to-memory self-test via `/dev/gdma_test`.
//! 2. Verifies the real I2S driver via `/dev/i2s_test` GPIO-matrix loopback:
//!    write a pattern, kernel runs simultaneous TX+RX DMA and compares.
//! 3. If the tests pass, plays a short C5 chime via `/dev/i2s0` (ES8311 codec).

extern crate libc;
extern crate librs;
extern crate rsrt;

mod mood_pcm;

use librs::syscall::Syscall;
use std::io::Write;
use std::os::unix::io::AsRawFd;

/// ioctl command for GDMA M2M self-test (must match the kernel constant).
const CMD_M2M_TEST: u32 = 0x1001;

const SAMPLE_RATE: u32 = 16_000;
const DURATION_SECS: u32 = 5;
const TONE_FREQ: f32 = 523.25; // C5
const AMPLITUDE: f32 = 0.8;

// I2S is configured with 32-bit slots, 16-bit data left-justified.
// Each stereo frame = 2 channels × 4 bytes = 8 bytes.
const CHANNELS: usize = 2;
const SLOT_BYTES: usize = 4;
const FRAME_BYTES: usize = CHANNELS * SLOT_BYTES;
const CHUNK_FRAMES: usize = 256;
const CHUNK_BYTES: usize = CHUNK_FRAMES * FRAME_BYTES;

/// Sine lookup table (one quadrant, 0..=90 degrees) and helper.
const SINE_TABLE: [u16; 64] = [
    0, 804, 1608, 2411, 3212, 4012, 4808, 5602,
    6393, 7180, 7962, 8740, 9512, 10279, 11039, 11793,
    12540, 13279, 14010, 14733, 15447, 16151, 16846, 17531,
    18205, 18868, 19520, 20160, 20788, 21403, 22006, 22595,
    23170, 23732, 24279, 24812, 25330, 25833, 26320, 26791,
    27246, 27684, 28106, 28511, 28899, 29269, 29622, 29957,
    30274, 30572, 30853, 31114, 31357, 31581, 31786, 31972,
    32138, 32286, 32413, 32522, 32610, 32679, 32729, 32758,
];

/// `phase` is 0..=0xFFFF mapping to 0..2π. Returns a 0..32767 sine value.
fn sine(phase: u32) -> u16 {
    let q = phase >> 14; // quadrant 0..3
    let i = ((phase >> 8) & 0x3F) as usize; // 0..63 within quadrant
    match q {
        0 => SINE_TABLE[i],
        1 => SINE_TABLE[63 - i],
        2 => 32768 - SINE_TABLE[i] as i32 as u16,
        _ => 32768 - SINE_TABLE[63 - i] as i32 as u16,
    }
}

/// Fill `buf` with `CHUNK_FRAMES` stereo frames of sine wave starting at
/// `phase`. Each 16-bit sample is left-justified into a 32-bit slot.
/// Returns the updated phase. `frame_index` and `total_frames` drive the
/// fade-in/fade-out envelope to avoid clicks at start/end.
fn fill_chunk(buf: &mut [u8], mut phase: u32, frame_index: usize, total_frames: usize, frames: usize) -> u32 {
    let fade_in = (SAMPLE_RATE as usize * 5) / 1000;   // 5ms
    let fade_out = (SAMPLE_RATE as usize * 20) / 1000;  // 20ms
    let phase_step = (TONE_FREQ / SAMPLE_RATE as f32 * 65536.0) as u32;

    for i in 0..frames {
        let abs_i = frame_index + i;
        let mut amp = AMPLITUDE;
        if abs_i < fade_in {
            amp *= abs_i as f32 / fade_in as f32;
        } else if abs_i >= total_frames.saturating_sub(fade_out) {
            amp *= (total_frames - abs_i) as f32 / fade_out as f32;
        }
        let s = ((amp * sine(phase) as f32) as i32) as u16 as u32;
        let sample = s << 16; // left-justify 16-bit data into 32-bit slot
        for ch in 0..CHANNELS {
            let off = (i * CHANNELS + ch) * SLOT_BYTES;
            buf[off..off + 4].copy_from_slice(&sample.to_le_bytes());
        }
        phase = phase.wrapping_add(phase_step);
    }
    phase
}

fn run_gdma_m2m_test() -> bool {
    println!("=== GDMA M2M Self-Test ===");

    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/gdma_test")
    {
        Ok(f) => f,
        Err(e) => {
            println!("  FAIL: cannot open /dev/gdma_test: {}", e);
            return false;
        }
    };

    // Trigger the M2M test via ioctl. The kernel runs the DMA transfer and
    // logs the result; the ioctl returns 0 on success.
    let ret = unsafe {
        librs::syscall::sys::Sys::ioctl(
            file.as_raw_fd(),
            CMD_M2M_TEST as libc::c_ulong,
            0 as *mut libc::c_void,
        )
    };

    if ret.is_ok() {
        println!("  PASSED: GDMA M2M transfer verified (see kernel log for details)");
        true
    } else {
        println!("  FAILED: GDMA M2M test failed (see kernel log for details)");
        false
    }
}

fn play_audio() -> std::io::Result<()> {
    println!("=== Audio Playback ===");
    let pcm = &mood_pcm::MOOD_PCM;
    println!(
        "Playing mood_pcm ({} bytes, 16-bit mono @ 16 kHz, ~{} s)",
        pcm.len(),
        pcm.len() / 2 / 16_000
    );

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/i2s0")?;

    // MOOD_PCM 位于 flash (.rodata)，SRAM 只有 512 KB。
    // 数据是 16-bit 单声道 PCM (2 字节/样本)，I2S 要求 32-bit slot /
    // 16-bit LJ 立体声 (8 字节/帧)。每个单声道样本复制为 L=R，各自
    // 左对齐到 32-bit slot。分段从 flash 读取转换到 DRAM 缓冲区。
    //
    // DMA 描述符 size 字段为 12 位，单次传输上限 4095 字节。
    // 取 4088 字节 (511 帧 × 8 字节，8 字节对齐) 最大化每次传输，
    // 减少 DMA 间隙，降低 FIFO 下溢导致的杂音。
    const LJ_CHUNK: usize = 4088; // 511 frames × 8 bytes
    const RAW_CHUNK: usize = LJ_CHUNK / 4; // 511 samples × 2 bytes = 1022
    let mut buf: Vec<u8> = vec![0u8; LJ_CHUNK];
    let mut offset = 0usize;

    while offset < pcm.len() {
        let raw_len = RAW_CHUNK.min(pcm.len() - offset);
        // 确保按完整样本对齐 (2 字节)
        let raw_len = raw_len & !1;
        if raw_len == 0 {
            break;
        }
        let samples = raw_len / 2;
        let lj_len = samples * 8;

        for s in 0..samples {
            let src = offset + s * 2;
            let dst = s * 8;
            buf[dst] = 0x00;
            buf[dst + 1] = 0x00;
            buf[dst + 2] = pcm[src];
            buf[dst + 3] = pcm[src + 1];
            buf[dst + 4] = 0x00;
            buf[dst + 5] = 0x00;
            buf[dst + 6] = pcm[src];
            buf[dst + 7] = pcm[src + 1];
        }

        file.write_all(&buf[..lj_len])?;
        offset += raw_len;
    }

    println!(
        "Playback complete ({} raw bytes, {} LJ bytes written)",
        offset,
        offset * 4
    );
    Ok(())
}

/// Test the normal I2S write path via `/dev/i2s0`.
///
/// This verifies that the I2S TX DMA path works independently of the
/// loopback logic. If this fails, the problem is in the I2S driver itself,
/// not in `loopback_transfer()`.
fn run_i2s0_write_test() -> bool {
    println!("=== I2S0 Write Test (/dev/i2s0) ===");

    let mut file = match std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/i2s0")
    {
        Ok(f) => f,
        Err(e) => {
            println!("  FAIL: cannot open /dev/i2s0: {}", e);
            return false;
        }
    };

    const LEN: usize = 256;
    let pattern: [u8; LEN] =
        core::array::from_fn(|i| (i as u8).wrapping_mul(3).wrapping_add(0x11));

    if let Err(e) = file.write_all(&pattern) {
        println!("  FAIL: /dev/i2s0 write failed: {}", e);
        return false;
    }
    println!("  wrote {} bytes to /dev/i2s0 (TX path OK)", LEN);
    true
}

/// Verify the real I2S driver via `/dev/i2s_test` GPIO-matrix loopback.
///
/// The kernel configures the GPIO matrix so that DOUT (TX data) and DIN
/// (RX data) are routed to the same GPIO pin. Writing to `/dev/i2s_test`
/// triggers a simultaneous TX/RX DMA transfer: the written pattern loops
/// back through the GPIO matrix into the RX buffer, and the kernel compares
/// the received bytes against the written pattern.
///
/// If `write()` returns Ok, the loopback transfer completed. The kernel
/// logs PASS/FAIL with mismatch details.
fn run_i2s_loopback_test() -> bool {
    println!("=== I2S Loopback Test (/dev/i2s_test) ===");

    let mut file = match std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/i2s_test")
    {
        Ok(f) => f,
        Err(e) => {
            println!("  FAIL: cannot open /dev/i2s_test: {}", e);
            return false;
        }
    };

    // Build a recognizable pattern (256 bytes).
    // Must exceed 130 bytes so RXEOF_NUM (0x40) triggers IN_SUC_EOF:
    // threshold = (BITS_MOD+1) * (RXEOF_NUM+1) = 16 * 65 = 1040 bits = 130 bytes.
    const LEN: usize = 256;
    let pattern: [u8; LEN] =
        core::array::from_fn(|i| (i as u8).wrapping_mul(3).wrapping_add(0x11));

    // Write triggers loopback_transfer() in the kernel: TX+RX DMA run
    // simultaneously, data loops back via GPIO matrix, kernel compares.
    if let Err(e) = file.write_all(&pattern) {
        println!("  FAIL: loopback write failed: {}", e);
        return false;
    }
    println!("  wrote {} bytes — see kernel log for PASS/FAIL", LEN);
    true
}

fn main() -> std::io::Result<()> {
    println!("BlueOS Audio + I2S Loopback Test Example");

    // Step 1: Verify GDMA functionality with M2M test.
    let gdma_ok = run_gdma_m2m_test();

    if !gdma_ok {
        println!("GDMA test failed — aborting.");
        return Ok(());
    }

    // Step 2: Test the normal I2S write path to /dev/i2s0.
    /*
    let i2s0_ok = run_i2s0_write_test();

    if !i2s0_ok {
        println!("/dev/i2s0 write failed — I2S driver itself has issues.");
        return Ok(());
    }
        */

    // Step 3: Verify the I2S driver via GPIO-matrix loopback.
    /*
    let i2s_ok = run_i2s_loopback_test();

    if !i2s_ok {
        println!("I2S loopback test failed — aborting audio playback.");
        return Ok(());
    }
    */

    // Step 4: Play audio via I2S (depends on GDMA being functional).
    play_audio()
}
