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

//! ESP32-C6 GDMA M2M self-test example — direct HAL-trait client + IRQ path.
//!
//! Mirrors the role of Linux `dmatest`: a *direct client* of the DMA
//! abstraction layer, not a user of a `/dev` node. The app allocates two
//! SRAM buffers, fills the source, asks the [`DmaChannel`] HAL trait to
//! copy one into the other, and verifies the result — all in app space,
//! without going through `/dev/gdma_test`'s ioctl.
//!
//! # Why a HAL-trait client (not an ioctl)
//!
//! The kernel's `/dev/gdma_test` device (`CMD_M2M_USER`) is a useful
//! "move these bytes" primitive, but it is *vendor-specific*: it calls
//! `Esp32c6GdmaChannel::m2m_transfer_impl` directly. By talking to the
//! [`DmaChannel`] trait instead, this app is decoupled from the concrete
//! controller — exactly the decoupling Linux's `dmatest` (a `dmaengine`
//! client of `device_prep_dma_memcpy`) enjoys. Swapping the SoC swaps
//! the `impl DmaChannel`; the app does not change.
//!
//! # Why userspace pointers reach the DMA engine
//!
//! ESP32-C6 is RV32IMAC with no MMU: `CONFIG_KERNEL_VIRT_OFFSET = 0x0`,
//! so `kernel_phys_to_virt(addr) == addr`. Userspace heap pointers (from
//! `posix_memalign` → `blueos::allocator`) ARE physical SRAM addresses.
//! The GDMA descriptor stores the full 32-bit `buffer` field and reads
//! it directly — no translation, no cache flush (SRAM is uncached on
//! ESP32-C6; only flash goes through the cache).
//!
//! # Interrupt-mode verification step
//!
//! The poll M2M transfer above does NOT exercise the DMA completion
//! interrupt path — it busy-waits on `IN_SUC_EOF`. The DMA interrupt
//! plumbing (board `GDMA_IN0_ISR` → driver `service_interrupt` RX block →
//! callback) is therefore unverified by the poll test. So the app adds a
//! second step: it issues `CMD_M2M_IRQ_TEST` to `/dev/gdma_test` via the
//! `librs` syscall layer. The kernel device owns the buffers and the
//! completion callback; the ioctl arms the M2M bridge on channel 0 (the
//! only channel whose IN0/OUT0 interrupts are board-wired) and reports
//! PASS only if the RX-side `IN_SUC_EOF` fires the IN0 ISR → callback →
//! flag within a bounded wait. This validates the whole interrupt path
//! end-to-end on real hardware.
//!
//! # Why the IRQ step goes through `/dev/gdma_test` (not the HAL trait)
//!
//! Each app is a standalone ELF and links its own copy of the `blueos_driver`
//! rlib, which means its own copy of the driver's `static CHAN_STATE`. The
//! board's ISR (in the kernel) reads the *kernel's* copy, so a callback
//! registered in app space would never fire. The `/dev/gdma_test` ioctl
//! runs in the kernel's address space and writes the *kernel's*
//! `CHAN_STATE[0].callback` — the one the IN0 ISR reads. That is why the
//! IRQ step cannot be a HAL-trait direct client like the poll step.
//!
//! Flash this example on an ESP32-C6 devkit and read the serial console.

extern crate alloc;
extern crate libc;
extern crate librs;
extern crate rsrt;

use blueos_driver::dma::esp32c6_gdma::Esp32c6GdmaChannel;
use blueos_hal::dma::DmaChannel;
use librs::{c_str::CStr, errno::Errno, syscall::{Sys, Syscall}};
use std::ffi::CString;

/// Transfer size. Must be ≤ 4095 (GDMA descriptor `size` field is 12 bits).
const XFER_SIZE: usize = 256;

/// ioctl request for `/dev/gdma_test` interrupt-mode M2M verification.
/// Must match `CMD_M2M_IRQ_TEST` in `kernel/kernel/src/devices/gdma_test.rs`.
/// The device owns its own buffers and callback; `arg` is ignored.
const CMD_M2M_IRQ_TEST: libc::c_ulong = 0x1003;

/// Run the interrupt-mode M2M verification via `/dev/gdma_test`. Returns
/// `Ok(())` if the IN0 interrupt path fired and the data copy matched,
/// `Err(io::Error)` otherwise. The poll step result is passed separately so
/// the caller can combine both before deciding the final exit status.
fn run_irq_test_step() -> std::io::Result<()> {
    // Open the kernel char device. Apps can't reach the kernel's
    // `CHAN_STATE` directly (each app links its own `blueos_driver` rlib
    // → own copy of the static; the kernel's ISR reads the kernel's copy),
    // so the IRQ step goes through the ioctl like the shell `ioctl`
    // command does.
    let cpath = CString::new("/dev/gdma_test")
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let cstr = unsafe { CStr::from_ptr(cpath.as_ptr()) };
    let fd = Sys::open(cstr, libc::O_RDWR, 0);
    if fd < 0 {
        return Err(std::io::Error::from_raw_os_error(-fd));
    }

    // Issue the IRQ test ioctl. The device arms the M2M bridge on channel
    // 0, registers a completion callback on the kernel's `CHAN_STATE[0]`,
    // and busy-waits for the IN0 ISR to set a flag. MIE is on during the
    // ioctl (SyscallGuard), so the interrupt is delivered mid-spin.
    let result = unsafe { Sys::ioctl(fd, CMD_M2M_IRQ_TEST, core::ptr::null_mut::<libc::c_void>()) };

    let _ = Sys::close(fd);

    match result {
        Ok(0) => {
            println!("[GDMA] IRQ DMA test PASSED: IN0 interrupt path validated");
            Ok(())
        }
        Ok(rc) => Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("ioctl returned rc={}", rc),
        )),
        Err(Errno(errno)) => {
            eprintln!("[GDMA] IRQ DMA test FAILED: errno={}", errno);
            Err(std::io::Error::from_raw_os_error(errno))
        }
    }
}

/// Allocate `size` bytes aligned to `align` via `posix_memalign` (the same
/// path the shell `alloc` command uses). Returns a raw pointer or an
/// `io::Error` on failure.
fn alloc_buffer(size: usize, align: usize) -> std::io::Result<*mut u8> {
    let mut ptr: *mut libc::c_void = core::ptr::null_mut();
    // SAFETY: posix_memalign is FFI; `ptr` outlives the call.
    let rc = unsafe { libc::posix_memalign(&mut ptr, align, size) };
    if rc != 0 || ptr.is_null() {
        return Err(std::io::Error::from_raw_os_error(libc::ENOMEM));
    }
    Ok(ptr as *mut u8)
}

/// Free a buffer allocated by `alloc_buffer`.
unsafe fn free_buffer(ptr: *mut u8) {
    // SAFETY: matched with a prior alloc_buffer; libc::free accepts NULL.
    unsafe { libc::free(ptr as *mut libc::c_void) }
}

fn main() -> std::io::Result<()> {
    println!("[GDMA] M2M DMA example starting (HAL-trait direct client)");

    // ── 1. Allocate wb (source) and rb (destination) ─────────────────
    // 4-byte alignment matches GDMA's preferred word access; both buffers
    // live in SRAM (the kernel heap region), so the DMA engine can reach
    // them with no address translation.
    let wb = alloc_buffer(XFER_SIZE, 4)?;
    let rb = alloc_buffer(XFER_SIZE, 4)?;
    println!(
        "[GDMA] wb (source)      = 0x{:08x}",
        wb as usize
    );
    println!(
        "[GDMA] rb (destination) = 0x{:08x}",
        rb as usize
    );

    // ── 2. Fill wb with a recognizable pattern; zero rb ──────────────
    // SAFETY: both buffers are XFER_SIZE bytes and valid.
    unsafe {
        for i in 0..XFER_SIZE {
            *wb.add(i) = (i as u8).wrapping_mul(7).wrapping_add(0xAB);
            *rb.add(i) = 0;
        }
    }

    // ── 3. Build mutable slices and ask the DMA engine to copy ───────
    // `DmaChannel::m2m_transfer` is the M2M entry point on the standard
    // HAL trait — the same place the slave outlink/inlink flow lives,
    // not a separate `DmaMemcpy` trait. Calling it through the trait
    // (not the inherent `m2m_transfer_impl`) keeps this app decoupled
    // from the concrete controller — mirroring how a Linux `dmatest`
    // client calls `dmaengine_prep_dma_memcpy` on a `dma_chan` without
    // naming the controller's driver.
    //
    // SAFETY: both buffers are XFER_SIZE bytes of accessible SRAM. No MMU
    // → the pointers are physical addresses the DMA engine reads directly.
    // The src side is only read by the DMA outlink, so the `&mut` aliasing
    // is benign in practice.
    let src = unsafe { core::slice::from_raw_parts_mut(wb, XFER_SIZE) };
    let dst = unsafe { core::slice::from_raw_parts_mut(rb, XFER_SIZE) };

    // Channel 2 avoids I2S TX/RX on channels 0/1. The channel type is a
    // zero-sized marker — `new()` asserts `CH < 3` at compile time.
    let dma_ok =
        match <Esp32c6GdmaChannel<2> as DmaChannel>::m2m_transfer(src, dst) {
            Ok(()) => true,
            Err(e) => {
                eprintln!(
                    "[GDMA] m2m_transfer failed: {:?} (GDMA hardware error?)",
                    e
                );
                false
            }
        };

    // ── 4. Read rb back and compare against wb ──────────────────────
    let mut passed = false;
    if dma_ok {
        // SAFETY: both buffers are XFER_SIZE bytes.
        let mut mismatches = 0usize;
        let mut first_mismatch = None;
        unsafe {
            for i in 0..XFER_SIZE {
                if *wb.add(i) != *rb.add(i) {
                    mismatches += 1;
                    if first_mismatch.is_none() {
                        first_mismatch = Some(i);
                    }
                }
            }
        }
        if mismatches == 0 {
            println!(
                "[GDMA] M2M DMA test PASSED: {} bytes copied correctly",
                XFER_SIZE
            );
            passed = true;
        } else {
            eprintln!(
                "[GDMA] M2M DMA test FAILED: {}/{} bytes mismatch, first at offset {}",
                mismatches,
                XFER_SIZE,
                first_mismatch.unwrap_or(0)
            );
        }
    }

    // ── 5. Cleanup: free buffers ────────────────────────────────────
    unsafe {
        free_buffer(wb);
        free_buffer(rb);
    }

    // ── 6. Interrupt-mode verification step ──────────────────────────
    // The poll step above only proved the M2M engine moves bytes. This
    // second step exercises the DMA completion *interrupt* path through
    // `/dev/gdma_test`: the kernel device arms the M2M bridge on channel
    // 0 (board-wired IN0/OUT0), the RX-side `IN_SUC_EOF` fires the board's
    // `GDMA_IN0_ISR` → `service_interrupt` → callback → flag. The ioctl
    // returns Ok only if the interrupt fired *and* the M2M copy matched.
    let irq_ok = match run_irq_test_step() {
        Ok(()) => true,
        Err(e) => {
            eprintln!("[GDMA] IRQ DMA test FAILED: {}", e);
            false
        }
    };

    // Final exit: both the poll M2M copy AND the IRQ path must pass.
    if passed && irq_ok {
        println!("[GDMA] All DMA tests PASSED (poll + interrupt)");
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "DMA test failed (poll or interrupt path)",
        ))
    }
}
