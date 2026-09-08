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

#![feature(cfg_boolean_literals)]
extern crate libm;
extern crate librs;
extern crate png;
extern crate rsrt;

mod app_window {
    include!(env!("SLINT_SDCARD_GENERATED"));
}
mod math;
mod text_pages;
mod website;

use crate::app_window::{FileEntry, MainWindow};
use librs::{c_str::CStr, syscall::Syscall};
use slint::platform::software_renderer::{LineBufferProvider, Rgb565Pixel};
use slint::platform::{PointerEventButton, WindowEvent};
use slint::{ComponentHandle, Model};
use std::cell::RefCell;
use std::io::{Error, ErrorKind, Read, Result as IoResult};
use std::rc::Rc;
use std::thread;
use text_pages::paginate_text;

const LCD_H_RES: u16 = 480;
const LCD_V_RES: u16 = 480;
const FRAME_DELAY_MS: libc::c_uint = 16;
const UI_THREAD_STACK_SIZE: usize = 64 * 1024;
// A 16-row RGB565 block is 15,360 bytes. It divides a 480-row frame evenly, reduces a full
// refresh to 30 writes, and preserves enough of the 64 KiB UI stack for Slint's renderer.
const FRAMEBUFFER_BATCH_LINES: usize = 16;
// Five larger rows fit between y=144 and y=400, inside the round panel's safe area.
const MAX_VISIBLE_ENTRIES: usize = 5;
// Keep text reads bounded: the UI retains only these bytes plus the generated display pages.
const TEXT_FILE_LIMIT: usize = 8 * 1024;
const PNG_MAX_DIMENSION: u32 = 480;
const PNG_DISPLAY_X: usize = 60;
const PNG_DISPLAY_Y: usize = 92;
const PNG_DISPLAY_MAX_WIDTH: u32 = 360;
const PNG_DISPLAY_MAX_HEIGHT: u32 = 326;
// Full-width batches match the framebuffer's linear layout. On the CO5300 this turns sixteen
// per-row panel updates into one aligned QSPI/GDMA transaction.
const PNG_FRAMEBUFFER_BATCH_LINES: usize = 16;
const PNG_VIEWER_PANEL_X: usize = 52;
const PNG_VIEWER_PANEL_WIDTH: usize = 376;
// DEFLATE requires a 32 KiB history window. Another 8 KiB lets the decoder make
// progress before the processed prefix is compacted. This replaces png::Reader's
// fixed 128 KiB unfiltering allocation and never retains a complete image.
const PNG_STREAM_BUFFER_SIZE: usize = 40 * 1024;
const PNG_INPUT_BUFFER_SIZE: usize = 2 * 1024;
const TOUCH_REPORT_SIZE: usize = 12;
const TOUCH_REPORT_VERSION: u8 = 1;
const TOUCH_DEVICE_PATH: &[u8] = b"/dev/cst9220\0";
const TOUCH_CONTROLLER_NAME: &str = "CST9220";
// CST9220 firmware reports coordinates in the mounted panel's logical direction.
// Do not mirror them again for the LCD controller's hardware scan direction.
const TOUCH_FLIP_X: bool = false;
const TOUCH_FLIP_Y: bool = false;
const TOUCH_SWAP_XY: bool = false;
// The esp32c6_devkitc_1 board mounts the FAT-formatted SD card at /data (see
// kernel/kernel/src/boards/esp32c6_devkitc_1/mod.rs, BLOCK_STORAGE_MOUNT_POINT).
const SD_ROOT: &str = "/data";
fn png_error(error: impl ToString) -> Error {
    Error::new(ErrorKind::InvalidData, error.to_string())
}

struct FileEntryInfo {
    name: String,
    is_dir: bool,
    size: u64,
}

/// Reads one directory and returns its entries with directories listed first, then files,
/// each group sorted by name. The display path is the absolute path.
fn read_directory(path: &str) -> IoResult<Vec<FileEntryInfo>> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        if website::is_internal_file(path, &entry.file_name().to_string_lossy()) {
            continue;
        }
        let metadata = entry.metadata()?;
        entries.push(FileEntryInfo {
            name: entry.file_name().to_string_lossy().into_owned(),
            is_dir: metadata.is_dir(),
            size: metadata.len(),
        });
    }

    entries.sort_by(|left, right| {
        // Directories first, then ASCII case-insensitive by name without
        // allocating lowercase copies on the constrained target heap.
        right
            .is_dir
            .cmp(&left.is_dir)
            .then_with(|| {
                left.name
                    .bytes()
                    .map(|byte| byte.to_ascii_lowercase())
                    .cmp(right.name.bytes().map(|byte| byte.to_ascii_lowercase()))
            })
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok(entries)
}

fn format_size(size: u64) -> String {
    if size >= 1024 * 1024 {
        format!("{:.1} MB", size as f64 / (1024.0 * 1024.0))
    } else if size >= 1024 {
        format!("{:.1} KB", size as f64 / 1024.0)
    } else {
        format!("{} B", size)
    }
}

fn is_text_file(name: &str) -> bool {
    match name.rsplit_once('.') {
        Some((_, extension)) => extension.eq_ignore_ascii_case("txt"),
        None => false,
    }
}

fn is_png_file(name: &str) -> bool {
    match name.rsplit_once('.') {
        Some((_, extension)) => extension.eq_ignore_ascii_case("png"),
        None => false,
    }
}

fn to_slint_entry(entry: &FileEntryInfo, directory: &str) -> FileEntry {
    let is_image = !entry.is_dir && is_png_file(&entry.name);
    FileEntry {
        name: website::display_name(directory, &entry.name).into(),
        is_dir: entry.is_dir,
        can_open: !entry.is_dir && (is_text_file(&entry.name) || is_image),
        is_image,
        size_text: format_size(entry.size).into(),
    }
}

fn read_text_pages(path: &str, file_size: u64) -> IoResult<(Vec<String>, bool)> {
    let file = std::fs::File::open(path)?;
    let mut bytes = Vec::with_capacity(file_size.min(TEXT_FILE_LIMIT as u64) as usize);
    file.take(TEXT_FILE_LIMIT as u64).read_to_end(&mut bytes)?;
    let truncated = file_size > bytes.len() as u64;
    let contents = String::from_utf8_lossy(&bytes);
    Ok((
        paginate_text(contents.trim_start_matches('\u{feff}')),
        truncated,
    ))
}

fn fit_png_dimensions(width: u32, height: u32) -> (u32, u32) {
    if width <= PNG_DISPLAY_MAX_WIDTH && height <= PNG_DISPLAY_MAX_HEIGHT {
        return (width, height);
    }

    if width as u64 * PNG_DISPLAY_MAX_HEIGHT as u64 >= height as u64 * PNG_DISPLAY_MAX_WIDTH as u64
    {
        (
            PNG_DISPLAY_MAX_WIDTH,
            (height as u64 * PNG_DISPLAY_MAX_WIDTH as u64 / width as u64).max(1) as u32,
        )
    } else {
        (
            (width as u64 * PNG_DISPLAY_MAX_HEIGHT as u64 / height as u64).max(1) as u32,
            PNG_DISPLAY_MAX_HEIGHT,
        )
    }
}

#[inline]
fn blend_png_channel(channel: u8, background: u8, alpha: u8) -> u8 {
    ((channel as u32 * alpha as u32 + background as u32 * (255 - alpha as u32) + 127) / 255) as u8
}

#[derive(Clone, Copy)]
struct PngHeader {
    width: u32,
    height: u32,
}

fn inspect_png(path: &str) -> IoResult<PngHeader> {
    let mut file = std::fs::File::open(path)?;
    let mut header = [0u8; 29];
    file.read_exact(&mut header)?;
    if header[..8] != [137, 80, 78, 71, 13, 10, 26, 10]
        || header[8..12] != [0, 0, 0, 13]
        || &header[12..16] != b"IHDR"
    {
        return Err(Error::new(ErrorKind::InvalidData, "invalid PNG header"));
    }

    let width = u32::from_be_bytes(header[16..20].try_into().unwrap());
    let height = u32::from_be_bytes(header[20..24].try_into().unwrap());
    if width == 0 || height == 0 || width > PNG_MAX_DIMENSION || height > PNG_MAX_DIMENSION {
        return Err(Error::new(
            ErrorKind::InvalidData,
            format!(
                "PNG dimensions {width}x{height} exceed {PNG_MAX_DIMENSION}x{PNG_MAX_DIMENSION}"
            ),
        ));
    }
    if header[26] != 0 || header[27] != 0 {
        return Err(Error::new(
            ErrorKind::InvalidData,
            "unsupported PNG compression or filter method",
        ));
    }
    if header[28] != 0 {
        return Err(Error::new(
            ErrorKind::InvalidData,
            "interlaced PNG is not supported",
        ));
    }

    Ok(PngHeader { width, height })
}

#[derive(Clone)]
struct PngRenderRequest {
    path: String,
    source_width: u32,
    source_height: u32,
    display_width: u32,
    display_height: u32,
}

#[derive(Default)]
struct PngRenderState {
    pending: Option<PngRenderRequest>,
    active: bool,
    ui: Option<slint::Weak<MainWindow>>,
}

type SharedPngRenderState = Rc<RefCell<PngRenderState>>;

struct PngStreamInfo {
    width: u32,
    height: u32,
    color_type: png::ColorType,
    bit_depth: png::BitDepth,
    row_length: usize,
    filter_bytes_per_pixel: usize,
    palette: Vec<u8>,
    transparency: Vec<u8>,
}

impl PngStreamInfo {
    fn from_decoder(info: &png::Info<'_>) -> IoResult<Self> {
        if info.width == 0
            || info.height == 0
            || info.width > PNG_MAX_DIMENSION
            || info.height > PNG_MAX_DIMENSION
        {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "PNG dimensions changed while decoding",
            ));
        }
        if info.interlaced {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "interlaced PNG is not supported",
            ));
        }
        if info.is_animated() {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "animated PNG is not supported",
            ));
        }

        let row_length = info.raw_row_length();
        if !(2..=(PNG_MAX_DIMENSION as usize * 8 + 1)).contains(&row_length) {
            return Err(Error::new(ErrorKind::InvalidData, "PNG row is too large"));
        }
        let palette = info.palette.as_deref().unwrap_or_default().to_vec();
        if info.color_type == png::ColorType::Indexed
            && (palette.is_empty() || palette.len() % 3 != 0)
        {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "indexed PNG has no valid palette",
            ));
        }

        Ok(Self {
            width: info.width,
            height: info.height,
            color_type: info.color_type,
            bit_depth: info.bit_depth,
            row_length,
            // PNG filters operate on bytes, not packed pixels. Sub-byte grayscale and
            // indexed formats therefore use one preceding byte for prediction.
            filter_bytes_per_pixel: info.bytes_per_pixel().max(1),
            palette,
            transparency: info.trns.as_deref().unwrap_or_default().to_vec(),
        })
    }
}

#[inline]
fn read_png_sample(row: &[u8], sample_index: usize, depth: png::BitDepth) -> Option<u16> {
    match depth {
        png::BitDepth::One | png::BitDepth::Two | png::BitDepth::Four => {
            let bits = depth as usize;
            let bit_offset = sample_index.checked_mul(bits)?;
            let byte = *row.get(bit_offset / 8)?;
            let shift = 8usize.checked_sub(bits + bit_offset % 8)?;
            Some(((byte >> shift) & ((1u8 << bits) - 1)) as u16)
        }
        png::BitDepth::Eight => row.get(sample_index).copied().map(u16::from),
        png::BitDepth::Sixteen => {
            let offset = sample_index.checked_mul(2)?;
            Some(u16::from_be_bytes([
                *row.get(offset)?,
                *row.get(offset + 1)?,
            ]))
        }
    }
}

#[inline]
fn png_sample_to_u8(sample: u16, depth: png::BitDepth) -> u8 {
    match depth {
        png::BitDepth::One => (sample * 255) as u8,
        png::BitDepth::Two => (sample * 85) as u8,
        png::BitDepth::Four => (sample * 17) as u8,
        png::BitDepth::Eight => sample as u8,
        png::BitDepth::Sixteen => ((sample as u32 + 128) / 257) as u8,
    }
}

#[inline]
fn png_transparent_sample(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_be_bytes([
        *bytes.get(offset)?,
        *bytes.get(offset + 1)?,
    ]))
}

#[inline]
fn png_pixel(info: &PngStreamInfo, row: &[u8], x: usize) -> IoResult<(u8, u8, u8, u8)> {
    let invalid = || Error::new(ErrorKind::InvalidData, "invalid PNG pixel data");
    match info.color_type {
        png::ColorType::Grayscale => {
            let gray = read_png_sample(row, x, info.bit_depth).ok_or_else(invalid)?;
            let alpha = if png_transparent_sample(&info.transparency, 0) == Some(gray) {
                0
            } else {
                255
            };
            let gray = png_sample_to_u8(gray, info.bit_depth);
            Ok((gray, gray, gray, alpha))
        }
        png::ColorType::Indexed => {
            let index = read_png_sample(row, x, info.bit_depth).ok_or_else(invalid)? as usize;
            let offset = index.checked_mul(3).ok_or_else(invalid)?;
            let red = *info.palette.get(offset).ok_or_else(invalid)?;
            let green = *info.palette.get(offset + 1).ok_or_else(invalid)?;
            let blue = *info.palette.get(offset + 2).ok_or_else(invalid)?;
            let alpha = info.transparency.get(index).copied().unwrap_or(255);
            Ok((red, green, blue, alpha))
        }
        png::ColorType::Rgb => {
            let sample = x.checked_mul(3).ok_or_else(invalid)?;
            let red = read_png_sample(row, sample, info.bit_depth).ok_or_else(invalid)?;
            let green = read_png_sample(row, sample + 1, info.bit_depth).ok_or_else(invalid)?;
            let blue = read_png_sample(row, sample + 2, info.bit_depth).ok_or_else(invalid)?;
            let transparent = png_transparent_sample(&info.transparency, 0) == Some(red)
                && png_transparent_sample(&info.transparency, 2) == Some(green)
                && png_transparent_sample(&info.transparency, 4) == Some(blue);
            Ok((
                png_sample_to_u8(red, info.bit_depth),
                png_sample_to_u8(green, info.bit_depth),
                png_sample_to_u8(blue, info.bit_depth),
                if transparent { 0 } else { 255 },
            ))
        }
        png::ColorType::GrayscaleAlpha => {
            let sample = x.checked_mul(2).ok_or_else(invalid)?;
            let gray = read_png_sample(row, sample, info.bit_depth).ok_or_else(invalid)?;
            let alpha = read_png_sample(row, sample + 1, info.bit_depth).ok_or_else(invalid)?;
            let gray = png_sample_to_u8(gray, info.bit_depth);
            Ok((gray, gray, gray, png_sample_to_u8(alpha, info.bit_depth)))
        }
        png::ColorType::Rgba => {
            let sample = x.checked_mul(4).ok_or_else(invalid)?;
            Ok((
                png_sample_to_u8(
                    read_png_sample(row, sample, info.bit_depth).ok_or_else(invalid)?,
                    info.bit_depth,
                ),
                png_sample_to_u8(
                    read_png_sample(row, sample + 1, info.bit_depth).ok_or_else(invalid)?,
                    info.bit_depth,
                ),
                png_sample_to_u8(
                    read_png_sample(row, sample + 2, info.bit_depth).ok_or_else(invalid)?,
                    info.bit_depth,
                ),
                png_sample_to_u8(
                    read_png_sample(row, sample + 3, info.bit_depth).ok_or_else(invalid)?,
                    info.bit_depth,
                ),
            ))
        }
    }
}

fn paeth_predictor(left: u8, above: u8, upper_left: u8) -> u8 {
    let left = left as i32;
    let above = above as i32;
    let upper_left = upper_left as i32;
    let estimate = left + above - upper_left;
    let left_distance = (estimate - left).abs();
    let above_distance = (estimate - above).abs();
    let diagonal_distance = (estimate - upper_left).abs();
    if left_distance <= above_distance && left_distance <= diagonal_distance {
        left as u8
    } else if above_distance <= diagonal_distance {
        above as u8
    } else {
        upper_left as u8
    }
}

fn unfilter_png_row(
    filter: u8,
    bytes_per_pixel: usize,
    previous: &[u8],
    row: &mut [u8],
) -> IoResult<()> {
    if previous.len() != row.len() {
        return Err(Error::new(ErrorKind::InvalidData, "invalid PNG row length"));
    }
    match filter {
        0 => {}
        1 => {
            for index in bytes_per_pixel..row.len() {
                row[index] = row[index].wrapping_add(row[index - bytes_per_pixel]);
            }
        }
        2 => {
            for (byte, above) in row.iter_mut().zip(previous.iter().copied()) {
                *byte = byte.wrapping_add(above);
            }
        }
        3 => {
            for index in 0..row.len() {
                let left = if index >= bytes_per_pixel {
                    row[index - bytes_per_pixel]
                } else {
                    0
                };
                row[index] =
                    row[index].wrapping_add(((left as u16 + previous[index] as u16) / 2) as u8);
            }
        }
        4 => {
            for index in 0..row.len() {
                let left = if index >= bytes_per_pixel {
                    row[index - bytes_per_pixel]
                } else {
                    0
                };
                let upper_left = if index >= bytes_per_pixel {
                    previous[index - bytes_per_pixel]
                } else {
                    0
                };
                row[index] =
                    row[index].wrapping_add(paeth_predictor(left, previous[index], upper_left));
            }
        }
        _ => return Err(Error::new(ErrorKind::InvalidData, "unknown PNG row filter")),
    }
    Ok(())
}

#[inline]
fn to_rgb565(red: u8, green: u8, blue: u8) -> Rgb565Pixel {
    Rgb565Pixel(((red as u16 & 0xf8) << 8) | ((green as u16 & 0xfc) << 3) | ((blue as u16) >> 3))
}

#[inline]
fn png_pixel_to_rgb565(red: u8, green: u8, blue: u8, alpha: u8) -> Rgb565Pixel {
    match alpha {
        255 => to_rgb565(red, green, blue),
        0 => to_rgb565(5, 9, 12),
        _ => to_rgb565(
            blend_png_channel(red, 5, alpha),
            blend_png_channel(green, 9, alpha),
            blend_png_channel(blue, 12, alpha),
        ),
    }
}

fn convert_png_row(
    info: &PngStreamInfo,
    row: &[u8],
    source_x_map: &[u16],
    output: &mut [Rgb565Pixel],
) -> IoResult<()> {
    if source_x_map.len() != output.len() {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "invalid PNG output row",
        ));
    }

    match (info.color_type, info.bit_depth) {
        (png::ColorType::Rgb, png::BitDepth::Eight) => {
            let required = info.width as usize * 3;
            if row.len() < required {
                return Err(Error::new(ErrorKind::InvalidData, "short PNG RGB row"));
            }

            if info.transparency.len() >= 6 {
                let transparent = (
                    png_transparent_sample(&info.transparency, 0).unwrap_or(u16::MAX),
                    png_transparent_sample(&info.transparency, 2).unwrap_or(u16::MAX),
                    png_transparent_sample(&info.transparency, 4).unwrap_or(u16::MAX),
                );
                for (pixel, source_x) in output.iter_mut().zip(source_x_map.iter().copied()) {
                    let offset = source_x as usize * 3;
                    let red = row[offset];
                    let green = row[offset + 1];
                    let blue = row[offset + 2];
                    let alpha =
                        if (u16::from(red), u16::from(green), u16::from(blue)) == transparent {
                            0
                        } else {
                            255
                        };
                    *pixel = png_pixel_to_rgb565(red, green, blue, alpha);
                }
            } else {
                for (pixel, source_x) in output.iter_mut().zip(source_x_map.iter().copied()) {
                    let offset = source_x as usize * 3;
                    *pixel = to_rgb565(row[offset], row[offset + 1], row[offset + 2]);
                }
            }
        }
        (png::ColorType::Rgba, png::BitDepth::Eight) => {
            let required = info.width as usize * 4;
            if row.len() < required {
                return Err(Error::new(ErrorKind::InvalidData, "short PNG RGBA row"));
            }
            for (pixel, source_x) in output.iter_mut().zip(source_x_map.iter().copied()) {
                let offset = source_x as usize * 4;
                *pixel = png_pixel_to_rgb565(
                    row[offset],
                    row[offset + 1],
                    row[offset + 2],
                    row[offset + 3],
                );
            }
        }
        _ => {
            for (pixel, source_x) in output.iter_mut().zip(source_x_map.iter().copied()) {
                let (red, green, blue, alpha) = png_pixel(info, row, source_x as usize)?;
                *pixel = png_pixel_to_rgb565(red, green, blue, alpha);
            }
        }
    }
    Ok(())
}

fn fill_rgb565(bytes: &mut [u8], color: Rgb565Pixel) {
    let encoded = color.0.to_be_bytes();
    for pixel in bytes.chunks_exact_mut(2) {
        pixel.copy_from_slice(&encoded);
    }
}

/// Collects direct PNG output into complete framebuffer rows.
///
/// A compact image rectangle cannot be submitted as several rows through the linear framebuffer
/// ABI because the bytes between rows are not contiguous. Supplying the surrounding viewer
/// background makes the rows contiguous, allowing the LCD driver to send one aligned rectangle
/// through QSPI/GDMA instead of reopening a transaction for every output row.
struct PngFramebufferWriter<'a> {
    fb: &'a mut FbFile,
    image_x: usize,
    image_y: usize,
    background_line: Vec<u8>,
    rgb565_batch: Vec<u8>,
    batch_first_line: usize,
    batch_line_count: usize,
    can_batch: bool,
}

impl<'a> PngFramebufferWriter<'a> {
    fn new(fb: &'a mut FbFile, request: &PngRenderRequest) -> Self {
        let line_bytes = LCD_H_RES as usize * 2;
        let can_batch = matches!(fb.pixel_format, PixelFormat::Rgb565)
            && fb.fixed_info.line_length as usize == line_bytes;
        let centered_y =
            PNG_DISPLAY_Y + (PNG_DISPLAY_MAX_HEIGHT as usize - request.display_height as usize) / 2;
        let image_y = if can_batch {
            // CO5300 accepts a multi-row rectangle directly when its origin and dimensions are
            // even. Moving an odd centered origin up by one pixel is visually insignificant.
            centered_y & !1
        } else {
            centered_y
        };

        let mut background_line = if can_batch {
            vec![0; line_bytes]
        } else {
            Vec::new()
        };
        if can_batch {
            fill_rgb565(&mut background_line, to_rgb565(9, 18, 23));
            let panel_start = PNG_VIEWER_PANEL_X * 2;
            let panel_end = (PNG_VIEWER_PANEL_X + PNG_VIEWER_PANEL_WIDTH) * 2;
            fill_rgb565(
                &mut background_line[panel_start..panel_end],
                to_rgb565(5, 9, 12),
            );
            background_line[panel_start..panel_start + 2]
                .copy_from_slice(&to_rgb565(41, 51, 77).0.to_be_bytes());
            background_line[panel_end - 2..panel_end]
                .copy_from_slice(&to_rgb565(41, 51, 77).0.to_be_bytes());
        }

        Self {
            fb,
            image_x: PNG_DISPLAY_X
                + (PNG_DISPLAY_MAX_WIDTH as usize - request.display_width as usize) / 2,
            image_y,
            background_line,
            rgb565_batch: if can_batch {
                vec![0; line_bytes * PNG_FRAMEBUFFER_BATCH_LINES]
            } else {
                Vec::new()
            },
            batch_first_line: 0,
            batch_line_count: 0,
            can_batch,
        }
    }

    fn flush(&mut self) -> IoResult<()> {
        if self.batch_line_count == 0 {
            return Ok(());
        }
        let byte_count = self.batch_line_count * LCD_H_RES as usize * 2;
        self.fb
            .draw_rgb565_rows(self.batch_first_line, &self.rgb565_batch[..byte_count])?;
        self.batch_line_count = 0;
        Ok(())
    }

    fn append_full_row(&mut self, line: usize, pixels: Option<&[Rgb565Pixel]>) -> IoResult<()> {
        if self.batch_line_count > 0 && line != self.batch_first_line + self.batch_line_count {
            self.flush()?;
        }
        if self.batch_line_count == 0 {
            self.batch_first_line = line;
        }

        let line_bytes = LCD_H_RES as usize * 2;
        let start = self.batch_line_count * line_bytes;
        let output = &mut self.rgb565_batch[start..start + line_bytes];
        output.copy_from_slice(&self.background_line);
        if let Some(pixels) = pixels {
            let image_start = self.image_x * 2;
            for (destination, pixel) in output[image_start..].chunks_exact_mut(2).zip(pixels.iter())
            {
                destination.copy_from_slice(&pixel.0.to_be_bytes());
            }
        }

        self.batch_line_count += 1;
        if self.batch_line_count == PNG_FRAMEBUFFER_BATCH_LINES {
            self.flush()?;
        }
        Ok(())
    }

    fn draw_row(&mut self, display_y: usize, pixels: &[Rgb565Pixel]) -> IoResult<()> {
        if self.can_batch {
            self.append_full_row(self.image_y + display_y, Some(pixels))
        } else {
            self.fb
                .draw_line(pixels, self.image_x, self.image_y + display_y)
        }
    }

    fn finish(mut self) -> IoResult<()> {
        if self.can_batch && self.batch_line_count & 1 != 0 {
            // Keep the final CO5300 rectangle even-height. The additional row contains the same
            // viewer background that Slint had already rendered at this position.
            let line = self.batch_first_line + self.batch_line_count;
            if line < LCD_V_RES as usize {
                self.append_full_row(line, None)?;
            }
        }
        self.flush()
    }
}

fn draw_png_source_row(
    writer: &mut PngFramebufferWriter<'_>,
    request: &PngRenderRequest,
    info: &PngStreamInfo,
    row: &[u8],
    source_y: u32,
    display_y: &mut u32,
    source_x_map: &[u16],
    output: &mut [Rgb565Pixel],
) -> IoResult<()> {
    while *display_y < request.display_height
        && *display_y as u64 * info.height as u64 / request.display_height as u64 == source_y as u64
    {
        convert_png_row(info, row, source_x_map, output)?;
        writer.draw_row(*display_y as usize, output)?;
        *display_y += 1;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn process_png_rows(
    writer: &mut PngFramebufferWriter<'_>,
    request: &PngRenderRequest,
    info: &PngStreamInfo,
    decode_buffer: &[u8],
    region: &png::UnfilterRegion,
    processed: &mut usize,
    source_y: &mut u32,
    display_y: &mut u32,
    previous_row: &mut Vec<u8>,
    current_row: &mut Vec<u8>,
    source_x_map: &[u16],
    output: &mut [Rgb565Pixel],
) -> IoResult<()> {
    while *source_y < info.height && region.filled.saturating_sub(*processed) >= info.row_length {
        let filter = decode_buffer[*processed];
        current_row.copy_from_slice(&decode_buffer[*processed + 1..*processed + info.row_length]);
        unfilter_png_row(
            filter,
            info.filter_bytes_per_pixel,
            previous_row,
            current_row,
        )?;
        draw_png_source_row(
            writer,
            request,
            info,
            current_row,
            *source_y,
            display_y,
            source_x_map,
            output,
        )?;
        core::mem::swap(previous_row, current_row);
        *processed += info.row_length;
        *source_y += 1;
    }
    Ok(())
}

fn compact_png_decode_buffer(
    decode_buffer: &mut [u8],
    region: &mut png::UnfilterRegion,
    processed: &mut usize,
) -> bool {
    if decode_buffer.len().saturating_sub(region.filled) >= 8 * 1024 {
        return false;
    }
    let discard = region.available.min(*processed);
    if discard == 0 {
        return false;
    }
    decode_buffer.copy_within(discard..region.filled, 0);
    region.available -= discard;
    region.filled -= discard;
    *processed -= discard;
    true
}

fn render_png_to_framebuffer(fb: &mut FbFile, request: &PngRenderRequest) -> IoResult<()> {
    let mut file = std::fs::File::open(&request.path)?;
    let mut decoder = png::StreamingDecoder::new();
    decoder.set_ignore_text_chunk(true);
    decoder.set_ignore_iccp_chunk(true);
    let _ = decoder.set_ignore_adler32(false);

    let mut decode_buffer = vec![0u8; PNG_STREAM_BUFFER_SIZE];
    let mut region = png::UnfilterRegion::default();
    let mut input = [0u8; PNG_INPUT_BUFFER_SIZE];
    let mut input_length = 0usize;
    let mut input_offset = 0usize;
    let mut processed = 0usize;
    let mut source_y = 0u32;
    let mut display_y = 0u32;
    let mut stream_info = None;
    let mut previous_row = Vec::new();
    let mut current_row = Vec::new();
    let mut source_x_map = Vec::new();
    let mut output = Vec::new();
    let mut writer = PngFramebufferWriter::new(fb, request);

    loop {
        if input_offset == input_length {
            input_length = file.read(&mut input)?;
            input_offset = 0;
            if input_length == 0 {
                return Err(Error::new(ErrorKind::UnexpectedEof, "incomplete PNG file"));
            }
        }

        let previous_filled = region.filled;
        let previous_source_y = source_y;
        let (consumed, decoded) = decoder
            .update(
                &input[input_offset..input_length],
                Some(&mut region.as_buf(&mut decode_buffer)),
            )
            .map_err(png_error)?;
        input_offset += consumed;

        if let png::Decoded::ChunkBegin(_, chunk) = &decoded {
            if *chunk == png::chunk::IDAT && stream_info.is_none() {
                let info = decoder
                    .info()
                    .ok_or_else(|| Error::new(ErrorKind::InvalidData, "PNG has no header"))?;
                let parsed = PngStreamInfo::from_decoder(info)?;
                if parsed.width != request.source_width || parsed.height != request.source_height {
                    return Err(Error::new(
                        ErrorKind::InvalidData,
                        "PNG changed after it was selected",
                    ));
                }
                let row_bytes = parsed.row_length - 1;
                previous_row.resize(row_bytes, 0);
                current_row.resize(row_bytes, 0);
                source_x_map.extend((0..request.display_width).map(|display_x| {
                    (display_x as u64 * parsed.width as u64 / request.display_width as u64) as u16
                }));
                output.resize(request.display_width as usize, Rgb565Pixel(0));
                stream_info = Some(parsed);
            }
        }

        if let Some(info) = stream_info.as_ref() {
            process_png_rows(
                &mut writer,
                request,
                info,
                &decode_buffer,
                &region,
                &mut processed,
                &mut source_y,
                &mut display_y,
                &mut previous_row,
                &mut current_row,
                &source_x_map,
                &mut output,
            )?;
        }

        let compacted = compact_png_decode_buffer(&mut decode_buffer, &mut region, &mut processed);
        if matches!(&decoded, png::Decoded::ImageDataFlushed) {
            let info = stream_info
                .as_ref()
                .ok_or_else(|| Error::new(ErrorKind::InvalidData, "PNG has no image data"))?;
            if source_y != info.height || display_y != request.display_height {
                return Err(Error::new(ErrorKind::UnexpectedEof, "incomplete PNG image"));
            }
            return writer.finish();
        }
        if matches!(&decoded, png::Decoded::ChunkComplete(chunk) if *chunk == png::chunk::IEND) {
            return Err(Error::new(ErrorKind::InvalidData, "PNG has no image data"));
        }
        if consumed == 0
            && region.filled == previous_filled
            && source_y == previous_source_y
            && !compacted
        {
            return Err(Error::new(
                ErrorKind::OutOfMemory,
                "PNG streaming buffer could not make progress",
            ));
        }
    }
}

#[derive(Clone, Copy)]
enum PixelFormat {
    Rgb565,
    Bgra8888,
}

impl PixelFormat {
    fn bytes_per_pixel(self) -> u32 {
        match self {
            Self::Rgb565 => 2,
            Self::Bgra8888 => 4,
        }
    }
}

struct FbFile {
    fd: libc::c_int,
    fixed_info: libc::fb_fix_screeninfo,
    variable_info: libc::fb_var_screeninfo,
    pixel_format: PixelFormat,
}

#[derive(Clone, Copy, Default)]
struct TouchPoint {
    status: u8,
    x: u16,
    y: u16,
}

struct TouchReport {
    touch_count: u8,
    points: [TouchPoint; 2],
}

impl TouchReport {
    fn decode(bytes: &[u8; TOUCH_REPORT_SIZE]) -> IoResult<Self> {
        if bytes[0] != TOUCH_REPORT_VERSION || bytes[1] > 2 {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "invalid CST9220 touch report",
            ));
        }

        let mut points = [TouchPoint::default(); 2];
        for (index, point) in points.iter_mut().enumerate() {
            let offset = 2 + index * 5;
            *point = TouchPoint {
                status: bytes[offset],
                x: u16::from_le_bytes([bytes[offset + 1], bytes[offset + 2]]),
                y: u16::from_le_bytes([bytes[offset + 3], bytes[offset + 4]]),
            };
        }

        Ok(Self {
            touch_count: bytes[1],
            points,
        })
    }

    fn active_point(&self) -> Option<TouchPoint> {
        if self.touch_count == 0 {
            return None;
        }

        self.points
            .iter()
            .copied()
            .find(|point| point.status != 0)
            .or(Some(self.points[0]))
    }
}

struct TouchFile {
    fd: libc::c_int,
    pressed: bool,
    last_x: f32,
    last_y: f32,
}

impl TouchFile {
    fn open() -> IoResult<Self> {
        let path = CStr::from_bytes_with_nul(TOUCH_DEVICE_PATH)
            .map_err(|_| Error::from_raw_os_error(libc::EINVAL))?;
        let fd = librs::syscall::sys::Sys::open(path, libc::O_RDONLY, 0);
        if fd < 0 {
            return Err(syscall_error(fd));
        }

        Ok(Self {
            fd,
            pressed: false,
            last_x: 0.0,
            last_y: 0.0,
        })
    }

    fn read_report(&self) -> IoResult<TouchReport> {
        let mut bytes = [0u8; TOUCH_REPORT_SIZE];
        match librs::syscall::sys::Sys::read(self.fd, &mut bytes) {
            Ok(TOUCH_REPORT_SIZE) => TouchReport::decode(&bytes),
            Ok(_) => Err(Error::new(ErrorKind::UnexpectedEof, "short CST9220 report")),
            Err(librs::errno::Errno(errno)) => Err(Error::from_raw_os_error(errno)),
        }
    }

    fn logical_position(point: TouchPoint) -> slint::LogicalPosition {
        let (raw_x, raw_y) = if TOUCH_SWAP_XY {
            (point.y, point.x)
        } else {
            (point.x, point.y)
        };
        let mut x = raw_x.min(LCD_H_RES - 1);
        let mut y = raw_y.min(LCD_V_RES - 1);
        if TOUCH_FLIP_X {
            x = LCD_H_RES - 1 - x;
        }
        if TOUCH_FLIP_Y {
            y = LCD_V_RES - 1 - y;
        }
        slint::LogicalPosition::new(x as f32, y as f32)
    }

    fn dispatch(
        &mut self,
        window: &slint::platform::software_renderer::MinimalSoftwareWindow,
    ) -> IoResult<()> {
        let report = self.read_report()?;
        match report.active_point() {
            Some(point) => {
                let position = Self::logical_position(point);
                if self.pressed {
                    if position.x != self.last_x || position.y != self.last_y {
                        window.dispatch_event(WindowEvent::PointerMoved { position });
                    }
                } else {
                    println!(
                        "{} press: raw=({}, {}), slint=({}, {})",
                        TOUCH_CONTROLLER_NAME, point.x, point.y, position.x, position.y
                    );
                    window.dispatch_event(WindowEvent::PointerPressed {
                        position,
                        button: PointerEventButton::Left,
                    });
                    self.pressed = true;
                }
                self.last_x = position.x;
                self.last_y = position.y;
            }
            None if self.pressed => {
                println!(
                    "{} release: slint=({}, {})",
                    TOUCH_CONTROLLER_NAME, self.last_x, self.last_y
                );
                window.dispatch_event(WindowEvent::PointerReleased {
                    position: slint::LogicalPosition::new(self.last_x, self.last_y),
                    button: PointerEventButton::Left,
                });
                self.pressed = false;
            }
            None => {}
        }
        Ok(())
    }
}

impl Drop for TouchFile {
    fn drop(&mut self) {
        let _ = librs::syscall::sys::Sys::close(self.fd);
    }
}

impl FbFile {
    fn open() -> IoResult<Self> {
        let path = CStr::from_bytes_with_nul(b"/dev/fb0\0")
            .map_err(|_| Error::from_raw_os_error(libc::EINVAL))?;
        let fd = librs::syscall::sys::Sys::open(path, libc::O_RDWR, 0);
        if fd < 0 {
            return Err(syscall_error(fd));
        }

        let mut fb = Self {
            fd,
            fixed_info: unsafe { core::mem::zeroed() },
            variable_info: unsafe { core::mem::zeroed() },
            pixel_format: PixelFormat::Rgb565,
        };

        fb.load_info().and_then(|_| fb.validate_format())?;

        Ok(fb)
    }

    fn load_info(&mut self) -> IoResult<()> {
        unsafe {
            ioctl(
                self.fd,
                libc::FBIOGET_FSCREENINFO,
                &mut self.fixed_info as *mut libc::fb_fix_screeninfo as *mut libc::c_void,
            )?;
            ioctl(
                self.fd,
                libc::FBIOGET_VSCREENINFO,
                &mut self.variable_info as *mut libc::fb_var_screeninfo as *mut libc::c_void,
            )?;
        }
        Ok(())
    }

    fn validate_format(&mut self) -> IoResult<()> {
        let info = &self.variable_info;
        let fixed = &self.fixed_info;
        let pixel_format = if is_rgb565(info) {
            PixelFormat::Rgb565
        } else if is_bgra8888(info) {
            PixelFormat::Bgra8888
        } else {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "unsupported framebuffer format",
            ));
        };
        let min_line_length = info
            .xres
            .checked_mul(pixel_format.bytes_per_pixel())
            .ok_or_else(|| Error::from_raw_os_error(libc::EINVAL))?;
        let min_size = fixed
            .line_length
            .checked_mul(info.yres)
            .ok_or_else(|| Error::from_raw_os_error(libc::EINVAL))?;

        if info.xres < LCD_H_RES as u32
            || info.yres < LCD_V_RES as u32
            || fixed.line_length < min_line_length
            || fixed.smem_len < min_size
        {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "unsupported framebuffer format",
            ));
        }

        self.pixel_format = pixel_format;
        Ok(())
    }

    fn draw_line(
        &mut self,
        pixels: &[Rgb565Pixel],
        origin_x: usize,
        origin_y: usize,
    ) -> IoResult<()> {
        let dst_offset = origin_y as u64 * self.fixed_info.line_length as u64
            + origin_x as u64 * self.pixel_format.bytes_per_pixel() as u64;
        if dst_offset > libc::off_t::MAX as u64 {
            return Err(Error::from_raw_os_error(libc::EINVAL));
        }

        let offset =
            librs::syscall::sys::Sys::lseek(self.fd, dst_offset as libc::off_t, libc::SEEK_SET);
        if offset < 0 {
            return Err(syscall_error(offset as libc::c_int));
        }

        match self.pixel_format {
            PixelFormat::Rgb565 => write_rgb565_line(self.fd, pixels),
            PixelFormat::Bgra8888 => write_bgra8888_line(self.fd, pixels),
        }
    }

    fn draw_rgb565_rows(&mut self, first_line: usize, bytes: &[u8]) -> IoResult<()> {
        debug_assert!(matches!(self.pixel_format, PixelFormat::Rgb565));
        let dst_offset = first_line as u64 * self.fixed_info.line_length as u64;
        if dst_offset > libc::off_t::MAX as u64 {
            return Err(Error::from_raw_os_error(libc::EINVAL));
        }

        let offset =
            librs::syscall::sys::Sys::lseek(self.fd, dst_offset as libc::off_t, libc::SEEK_SET);
        if offset < 0 {
            return Err(syscall_error(offset as libc::c_int));
        }

        write_all(self.fd, bytes)
    }
}

/// Renders into one reusable scanline and batches complete RGB565 rows instead of allocating
/// a full-frame pixel buffer.
///
/// This keeps the software renderer's SRAM usage low. Slint 1.17 does not support `Path`
/// items in `render_by_line`, so this backend intentionally does not accept `Path` items.
struct FbLineBuffer<'a> {
    fb: &'a mut FbFile,
    pixels: [Rgb565Pixel; LCD_H_RES as usize],
    rgb565_batch: [u8; LCD_H_RES as usize * 2 * FRAMEBUFFER_BATCH_LINES],
    batch_first_line: usize,
    batch_line_count: usize,
    result: &'a mut IoResult<()>,
}

impl<'a> FbLineBuffer<'a> {
    fn new(fb: &'a mut FbFile, result: &'a mut IoResult<()>) -> Self {
        Self {
            fb,
            pixels: [Rgb565Pixel(0); LCD_H_RES as usize],
            rgb565_batch: [0; LCD_H_RES as usize * 2 * FRAMEBUFFER_BATCH_LINES],
            batch_first_line: 0,
            batch_line_count: 0,
            result,
        }
    }

    fn flush_batch(&mut self) {
        if self.batch_line_count == 0 {
            return;
        }

        let byte_count = self.batch_line_count * LCD_H_RES as usize * 2;
        if self.result.is_ok() {
            *self.result = self
                .fb
                .draw_rgb565_rows(self.batch_first_line, &self.rgb565_batch[..byte_count]);
        }
        self.batch_line_count = 0;
    }
}

impl LineBufferProvider for FbLineBuffer<'_> {
    type TargetPixel = Rgb565Pixel;

    fn process_line(
        &mut self,
        line: usize,
        range: core::ops::Range<usize>,
        render_fn: impl FnOnce(&mut [Self::TargetPixel]),
    ) {
        let pixel_count = range.len();
        debug_assert!(pixel_count <= self.pixels.len());
        let can_batch = matches!(self.fb.pixel_format, PixelFormat::Rgb565)
            && range.start == 0
            && pixel_count == LCD_H_RES as usize;
        if !can_batch
            || (self.batch_line_count > 0 && line != self.batch_first_line + self.batch_line_count)
        {
            self.flush_batch();
        }

        let pixels = &mut self.pixels[..pixel_count];
        render_fn(pixels);

        if self.result.is_err() {
            return;
        }

        if can_batch {
            if self.batch_line_count == 0 {
                self.batch_first_line = line;
            }
            let line_bytes = LCD_H_RES as usize * 2;
            let batch_offset = self.batch_line_count * line_bytes;
            for (index, pixel) in pixels.iter().enumerate() {
                let pixel_bytes = pixel.0.to_be_bytes();
                self.rgb565_batch[batch_offset + index * 2] = pixel_bytes[0];
                self.rgb565_batch[batch_offset + index * 2 + 1] = pixel_bytes[1];
            }
            self.batch_line_count += 1;
            if self.batch_line_count == FRAMEBUFFER_BATCH_LINES {
                self.flush_batch();
            }
        } else {
            *self.result = self.fb.draw_line(pixels, range.start, line);
        }
    }
}

impl Drop for FbLineBuffer<'_> {
    fn drop(&mut self) {
        self.flush_batch();
    }
}

impl Drop for FbFile {
    fn drop(&mut self) {
        let _ = librs::syscall::sys::Sys::close(self.fd);
    }
}

unsafe fn ioctl(fd: libc::c_int, request: libc::c_ulong, arg: *mut libc::c_void) -> IoResult<()> {
    match librs::syscall::sys::Sys::ioctl(fd, request, arg) {
        Ok(ret) if ret < 0 => Err(syscall_error(ret)),
        Ok(_) => Ok(()),
        Err(librs::errno::Errno(errno)) => Err(Error::from_raw_os_error(errno)),
    }
}

fn write_all(fd: libc::c_int, mut buf: &[u8]) -> IoResult<()> {
    while !buf.is_empty() {
        match librs::syscall::sys::Sys::write(fd, buf) {
            Ok(0) => {
                return Err(Error::new(
                    ErrorKind::WriteZero,
                    "failed to write framebuffer",
                ))
            }
            Ok(size) => buf = &buf[size..],
            Err(librs::errno::Errno(errno)) => return Err(Error::from_raw_os_error(errno)),
        }
    }

    Ok(())
}

fn write_rgb565_line(fd: libc::c_int, pixels: &[Rgb565Pixel]) -> IoResult<()> {
    let mut bytes = [0; LCD_H_RES as usize * 2];
    debug_assert!(pixels.len() * 2 <= bytes.len());

    for (index, pixel) in pixels.iter().enumerate() {
        let pixel_bytes = pixel.0.to_be_bytes();
        bytes[index * 2] = pixel_bytes[0];
        bytes[index * 2 + 1] = pixel_bytes[1];
    }

    write_all(fd, &bytes[..pixels.len() * 2])
}

fn write_bgra8888_line(fd: libc::c_int, pixels: &[Rgb565Pixel]) -> IoResult<()> {
    let mut bytes = [0; LCD_H_RES as usize * 4];
    debug_assert!(pixels.len() * 4 <= bytes.len());

    for (index, pixel) in pixels.iter().enumerate() {
        let [b0, b1] = pixel.0.to_be_bytes();
        let rgb = u16::from_be_bytes([b0, b1]);
        bytes[index * 4] = ((rgb & 0x001f) << 3) as u8;
        bytes[index * 4 + 1] = ((rgb & 0x07e0) >> 3) as u8;
        bytes[index * 4 + 2] = ((rgb & 0xf800) >> 8) as u8;
        bytes[index * 4 + 3] = 0xff;
    }

    write_all(fd, &bytes[..pixels.len() * 4])
}

fn is_rgb565(info: &libc::fb_var_screeninfo) -> bool {
    info.bits_per_pixel == 16
        && info.red.offset == 11
        && info.red.length == 5
        && info.green.offset == 5
        && info.green.length == 6
        && info.blue.offset == 0
        && info.blue.length == 5
}

fn is_bgra8888(info: &libc::fb_var_screeninfo) -> bool {
    info.bits_per_pixel == 32
        && info.red.offset == 16
        && info.red.length == 8
        && info.green.offset == 8
        && info.green.length == 8
        && info.blue.offset == 0
        && info.blue.length == 8
}

fn syscall_error(ret: libc::c_int) -> Error {
    if ret == -1 {
        Error::last_os_error()
    } else {
        Error::from_raw_os_error(-ret)
    }
}

fn uptime_millis() -> u128 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };

    let ret = unsafe { librs::time::clock_gettime(librs::time::CLOCK_MONOTONIC, &mut ts) };

    if ret != 0 {
        return 0;
    }

    (ts.tv_sec as u128) * 1000 + (ts.tv_nsec as u128) / 1_000_000
}

struct BluekernelBackend {
    window: RefCell<Option<Rc<slint::platform::software_renderer::MinimalSoftwareWindow>>>,
    png_render_state: SharedPngRenderState,
}

impl BluekernelBackend {
    fn new(png_render_state: SharedPngRenderState) -> Self {
        Self {
            window: RefCell::new(None),
            png_render_state,
        }
    }
}

impl slint::platform::Platform for BluekernelBackend {
    fn create_window_adapter(
        &self,
    ) -> Result<Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
        let window = slint::platform::software_renderer::MinimalSoftwareWindow::new(
            // Entry rows change on navigation. ReusedBuffer retains partial-rendering
            // caches for old text and eventually fragments the small ESP32-C6 heap. NewBuffer
            // clears that cache before each requested frame and redraws the full framebuffer.
            slint::platform::software_renderer::RepaintBufferType::NewBuffer,
        );
        window.set_size(slint::PhysicalSize::new(LCD_H_RES as u32, LCD_V_RES as u32));
        self.window.replace(Some(window.clone()));
        Ok(window)
    }

    fn duration_since_start(&self) -> std::time::Duration {
        let t = uptime_millis();
        std::time::Duration::from_millis(t as u64)
    }

    fn run_event_loop(&self) -> Result<(), slint::PlatformError> {
        let mut fb = FbFile::open().map_err(|err| slint::PlatformError::Other(err.to_string()))?;
        let mut touch = match TouchFile::open() {
            Ok(touch) => Some(touch),
            Err(error) => {
                println!("Failed to open /dev/cst9220: {error}");
                None
            }
        };
        let mut touch_error_reported = false;

        loop {
            slint::platform::update_timers_and_animations();

            if let Some(window) = self.window.borrow().clone() {
                let mut draw_result = Ok(());
                let mut frame_was_drawn = false;
                if !self.png_render_state.borrow().active {
                    window.draw_if_needed(|renderer| {
                        frame_was_drawn = true;
                        // Render line-by-line to avoid a full-frame RGB565 allocation. This saves
                        // substantial SRAM, at the cost of not supporting Slint `Path` items.
                        renderer.render_by_line(FbLineBuffer::new(&mut fb, &mut draw_result));
                    });
                }
                draw_result.map_err(|err| slint::PlatformError::Other(err.to_string()))?;

                // First let Slint draw the viewer chrome, then stream the image directly into
                // its content rectangle. While the image is active Slint redraws are held back;
                // otherwise a full software-renderer pass would overwrite the direct pixels.
                let request = if frame_was_drawn {
                    self.png_render_state.borrow_mut().pending.take()
                } else {
                    None
                };
                if let Some(request) = request {
                    println!(
                        "[SDCARD] decoding PNG {} ({}x{} -> {}x{})",
                        request.path,
                        request.source_width,
                        request.source_height,
                        request.display_width,
                        request.display_height
                    );
                    let render_result = render_png_to_framebuffer(&mut fb, &request);
                    let ui_weak = {
                        let mut state = self.png_render_state.borrow_mut();
                        state.active = render_result.is_ok();
                        state.ui.clone()
                    };
                    match render_result {
                        Ok(()) => println!("[SDCARD] PNG display complete: {}", request.path),
                        Err(error) => {
                            println!("[SDCARD] PNG display failed {}: {error}", request.path);
                            if let Some(ui) = ui_weak.and_then(|ui| ui.upgrade()) {
                                ui.set_image_viewer_status(format!("FAILED: {error}").into());
                            }
                        }
                    }
                }

                // Poll after drawing so a stalled I2C bus cannot prevent the
                // initial UI frame from reaching the panel.
                if let Some(touch) = touch.as_mut() {
                    match touch.dispatch(&window) {
                        Ok(()) => touch_error_reported = false,
                        Err(error) if !touch_error_reported => {
                            println!("Failed to read CST9220 touch data: {error}");
                            touch_error_reported = true;
                        }
                        Err(_) => {}
                    }
                }

                let _ = librs::time::msleep(FRAME_DELAY_MS);
            } else {
                let _ = librs::time::msleep(FRAME_DELAY_MS);
            }
        }
    }
}

fn replace_entry_rows(ui: &MainWindow, rows: Vec<FileEntry>) {
    let model = ui.get_entries();
    if let Some(model) = model.as_any().downcast_ref::<slint::VecModel<FileEntry>>() {
        // Update rows individually so Slint keeps the existing repeater instances
        // and their rendering state. A model reset destroys and recreates all row
        // item trees, which needs a large contiguous allocation on every navigation.
        let old_count = model.row_count();
        let new_count = rows.len();
        for (index, row) in rows.into_iter().enumerate() {
            if index < old_count {
                if model.row_data(index).as_ref() != Some(&row) {
                    model.set_row_data(index, row);
                }
            } else {
                model.push(row);
            }
        }
        for index in (new_count..old_count).rev() {
            model.remove(index);
        }
    } else {
        ui.set_entries(slint::ModelRc::new(slint::VecModel::from(rows)));
    }
}

struct SdBrowser {
    current_path: String,
    entries: Vec<FileEntryInfo>,
    scroll_offset: usize,
    text_pages: Vec<String>,
    text_page: usize,
    text_truncated: bool,
    png_render_state: SharedPngRenderState,
}

impl SdBrowser {
    fn new(png_render_state: SharedPngRenderState) -> Self {
        Self {
            current_path: SD_ROOT.to_string(),
            entries: Vec::new(),
            scroll_offset: 0,
            text_pages: Vec::new(),
            text_page: 0,
            text_truncated: false,
            png_render_state,
        }
    }

    fn update_visible_entries(&self, ui: &MainWindow) {
        let total = self.entries.len();
        let visible_end = (self.scroll_offset + MAX_VISIBLE_ENTRIES).min(total);
        let visible = self.entries[self.scroll_offset..visible_end]
            .iter()
            .map(|entry| to_slint_entry(entry, &self.current_path))
            .collect();

        replace_entry_rows(ui, visible);
        ui.set_total_count(total as i32);
        ui.set_visible_count((visible_end - self.scroll_offset) as i32);
        ui.set_scroll_offset(self.scroll_offset as i32);
        ui.set_status_text(if total == 0 {
            "Empty directory".into()
        } else {
            format!("{} items", total).into()
        });
    }

    fn refresh(&mut self, ui: &MainWindow) {
        ui.set_path_text(self.current_path.clone().into());
        match read_directory(&self.current_path) {
            Ok(entries) => {
                println!(
                    "[SDCARD] loaded {} entries from {}",
                    entries.len(),
                    self.current_path
                );
                self.entries = entries;
                self.scroll_offset = self
                    .scroll_offset
                    .min(self.entries.len().saturating_sub(MAX_VISIBLE_ENTRIES));
                ui.set_read_error(false);
                self.update_visible_entries(ui);
            }
            Err(error) => {
                println!("[SDCARD] failed to read {}: {error}", self.current_path);
                self.entries.clear();
                self.scroll_offset = 0;
                replace_entry_rows(ui, Vec::new());
                ui.set_total_count(0);
                ui.set_visible_count(0);
                ui.set_scroll_offset(0);
                ui.set_read_error(true);
                ui.set_status_text(format!("Read failed: {error}").into());
            }
        }
    }

    fn select(&mut self, ui: &MainWindow, index: usize) {
        let Some(entry) = self.entries.get(self.scroll_offset + index) else {
            return;
        };
        let name = entry.name.clone();
        let is_dir = entry.is_dir;
        let size = entry.size;

        let path = if self.current_path.ends_with('/') {
            format!("{}{}", self.current_path, name)
        } else {
            format!("{}/{}", self.current_path, name)
        };

        if !is_dir {
            if is_text_file(&name) {
                self.open_text_file(ui, &path, &name, size);
            } else if is_png_file(&name) {
                self.open_png_file(ui, &path, &name);
            } else {
                ui.set_status_text("Only .txt and .png files can be opened".into());
            }
            return;
        }

        self.current_path = path;
        self.scroll_offset = 0;
        self.refresh(ui);
    }

    fn open_text_file(&mut self, ui: &MainWindow, path: &str, name: &str, size: u64) {
        match read_text_pages(path, size) {
            Ok((pages, truncated)) => {
                println!(
                    "[SDCARD] opened text file {} ({} bytes, {} pages{})",
                    path,
                    size,
                    pages.len(),
                    if truncated { ", truncated" } else { "" }
                );
                self.text_pages = pages;
                self.text_page = 0;
                self.text_truncated = truncated;
                ui.set_text_viewer_title(website::display_name(&self.current_path, name).into());
                ui.set_text_viewer_path(path.into());
                ui.set_text_viewer_open(true);
                self.update_text_viewer(ui);
            }
            Err(error) => {
                println!("[SDCARD] failed to read text file {path}: {error}");
                ui.set_status_text(format!("Open failed: {error}").into());
            }
        }
    }

    fn update_text_viewer(&self, ui: &MainWindow) {
        let Some(page) = self.text_pages.get(self.text_page) else {
            return;
        };
        ui.set_text_viewer_content(page.as_str().into());
        ui.set_text_viewer_page(
            format!(
                "{} / {}{}",
                self.text_page + 1,
                self.text_pages.len(),
                if self.text_truncated { "  LIMIT" } else { "" }
            )
            .into(),
        );
        ui.set_text_viewer_has_previous(self.text_page > 0);
        ui.set_text_viewer_has_next(self.text_page + 1 < self.text_pages.len());
    }

    fn close_text_viewer(&mut self, ui: &MainWindow) {
        ui.set_text_viewer_open(false);
        ui.set_text_viewer_content("".into());
        ui.set_text_viewer_has_previous(false);
        ui.set_text_viewer_has_next(false);
        self.text_pages.clear();
        self.text_page = 0;
        self.text_truncated = false;
        println!("[SDCARD] TXT back to {}", self.current_path);
    }

    fn turn_text_page(&mut self, ui: &MainWindow, delta: isize) {
        if self.text_pages.is_empty() {
            return;
        }
        let last_page = self.text_pages.len() - 1;
        let new_page = if delta < 0 {
            self.text_page.saturating_sub(delta.unsigned_abs())
        } else {
            (self.text_page + delta as usize).min(last_page)
        };
        if new_page != self.text_page {
            self.text_page = new_page;
            self.update_text_viewer(ui);
            println!(
                "[SDCARD] text page {} / {}",
                self.text_page + 1,
                self.text_pages.len()
            );
        }
    }

    fn open_png_file(&mut self, ui: &MainWindow, path: &str, name: &str) {
        match inspect_png(path) {
            Ok(header) => {
                let (display_width, display_height) =
                    fit_png_dimensions(header.width, header.height);
                ui.set_image_viewer_title(website::display_name(&self.current_path, name).into());
                ui.set_image_viewer_path(path.into());
                ui.set_image_viewer_status(
                    format!("{} x {}", display_width, display_height).into(),
                );
                ui.set_image_viewer_open(true);
                let mut state = self.png_render_state.borrow_mut();
                state.active = false;
                state.pending = Some(PngRenderRequest {
                    path: path.to_string(),
                    source_width: header.width,
                    source_height: header.height,
                    display_width,
                    display_height,
                });
            }
            Err(error) => {
                println!("[SDCARD] rejected PNG {path}: {error}");
                ui.set_status_text(format!("PNG failed: {error}").into());
            }
        }
    }

    fn close_image_viewer(&mut self, ui: &MainWindow) {
        let mut state = self.png_render_state.borrow_mut();
        state.pending = None;
        state.active = false;
        ui.set_image_viewer_open(false);
        ui.window().request_redraw();
        println!("[SDCARD] PNG back to {}", self.current_path);
    }

    fn go_up(&mut self, ui: &MainWindow) {
        if self.current_path == SD_ROOT {
            return;
        }
        match self.current_path.rfind('/') {
            Some(0) => self.current_path = SD_ROOT.to_string(),
            Some(index) => self.current_path.truncate(index),
            None => self.current_path = SD_ROOT.to_string(),
        }
        self.scroll_offset = 0;
        println!("[SDCARD] up to {}", self.current_path);
        self.refresh(ui);
    }

    fn scroll(&mut self, ui: &MainWindow, delta: isize) {
        let max_offset = self.entries.len().saturating_sub(MAX_VISIBLE_ENTRIES);
        let new_offset = if delta < 0 {
            self.scroll_offset.saturating_sub(delta.unsigned_abs())
        } else {
            (self.scroll_offset + delta as usize).min(max_offset)
        };
        if new_offset == self.scroll_offset {
            return;
        }
        self.scroll_offset = new_offset;
        self.update_visible_entries(ui);
    }
}

fn run_slint_ui() -> IoResult<()> {
    println!("Starting Slint SD card browser UI");

    let png_render_state = Rc::new(RefCell::new(PngRenderState::default()));
    slint::platform::set_platform(Box::new(BluekernelBackend::new(png_render_state.clone())))
        .map_err(|err| Error::new(ErrorKind::Other, err.to_string()))?;
    let ui = MainWindow::new().map_err(|err| Error::new(ErrorKind::Other, err.to_string()))?;
    ui.set_entries(slint::ModelRc::new(slint::VecModel::default()));
    png_render_state.borrow_mut().ui = Some(ui.as_weak());

    let browser = Rc::new(RefCell::new(SdBrowser::new(png_render_state)));
    let ui_weak = ui.as_weak();

    {
        let callback_browser = browser.clone();
        ui.on_refresh_requested(move || {
            if let Some(ui) = ui_weak.upgrade() {
                callback_browser.borrow_mut().refresh(&ui);
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        let callback_browser = browser.clone();
        ui.on_entry_selected(move |index| {
            if let Some(ui) = ui_weak.upgrade() {
                callback_browser.borrow_mut().select(&ui, index as usize);
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        let callback_browser = browser.clone();
        ui.on_up_requested(move || {
            if let Some(ui) = ui_weak.upgrade() {
                callback_browser.borrow_mut().go_up(&ui);
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        let callback_browser = browser.clone();
        ui.on_scroll_up(move || {
            if let Some(ui) = ui_weak.upgrade() {
                callback_browser.borrow_mut().scroll(&ui, -1);
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        let callback_browser = browser.clone();
        ui.on_scroll_down(move || {
            if let Some(ui) = ui_weak.upgrade() {
                callback_browser.borrow_mut().scroll(&ui, 1);
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        let callback_browser = browser.clone();
        ui.on_close_text_viewer(move || {
            if let Some(ui) = ui_weak.upgrade() {
                callback_browser.borrow_mut().close_text_viewer(&ui);
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        let callback_browser = browser.clone();
        ui.on_previous_text_page(move || {
            if let Some(ui) = ui_weak.upgrade() {
                callback_browser.borrow_mut().turn_text_page(&ui, -1);
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        let callback_browser = browser.clone();
        ui.on_next_text_page(move || {
            if let Some(ui) = ui_weak.upgrade() {
                callback_browser.borrow_mut().turn_text_page(&ui, 1);
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        let callback_browser = browser.clone();
        ui.on_close_image_viewer(move || {
            if let Some(ui) = ui_weak.upgrade() {
                callback_browser.borrow_mut().close_image_viewer(&ui);
            }
        });
    }

    ui.show()
        .map_err(|err| Error::new(ErrorKind::Other, err.to_string()))?;

    // Load the root directory after the first frame reaches the panel, so an SD read
    // (which blocks the event loop on SPI) cannot delay startup rendering.
    let initial_ui = ui.as_weak();
    let load_timer = slint::Timer::default();
    load_timer.start(
        slint::TimerMode::SingleShot,
        std::time::Duration::from_millis(100),
        move || {
            if let Some(ui) = initial_ui.upgrade() {
                let demo_path = website::install();
                let mut browser = browser.borrow_mut();
                match demo_path {
                    Ok(path) => browser.current_path = path,
                    Err(error) => {
                        println!("[SDCARD-CONTENT] failed to install website content: {error}")
                    }
                }
                browser.refresh(&ui);
            }
        },
    );

    slint::run_event_loop().map_err(|err| Error::new(ErrorKind::Other, err.to_string()))
}

fn main() -> IoResult<()> {
    let ui_thread = thread::Builder::new()
        .name("slint-ui".to_string())
        .stack_size(UI_THREAD_STACK_SIZE)
        .spawn(run_slint_ui)?;

    ui_thread
        .join()
        .map_err(|_| Error::new(ErrorKind::Other, "slint ui thread panicked"))?
}
