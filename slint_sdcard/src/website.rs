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

//! Installs small, website-sourced TXT/PNG samples from firmware Flash onto the SD card.

use std::fs::{self, File};
use std::io::{Error, ErrorKind, Read, Result, Write};

pub const ROOT: &str = "/data/blueos-kernel";
const CLEANUP_MARKER: &str = "/data/blueos-kernel/.installed-v2.txt";
const CLEANUP_RECORD: &[u8] =
    b"BlueOS reading samples v2\nRetired inventoried BIN samples and three decorative icons.\n";

// These paths and lengths were read from this board before enabling the migration. Do not
// replace this list with a recursive delete: later SD-card contents may belong to the user.
const LEGACY_SAMPLES: &[(&str, u64)] = &[
    ("/data/slint-demo/sample-1-f991.bin", 235),
    ("/data/slint-demo/sample-2-33e6.bin", 136),
    ("/data/slint-demo/sample-3-32f8.bin", 257),
    ("/data/blueos-kernel/12-safety.png", 4716),
    ("/data/blueos-kernel/13-lightweight.png", 5147),
    ("/data/blueos-kernel/14-portability.png", 5618),
    ("/data/blueos-kernel/installed-v1.txt", 148),
];

const ASSETS: &[(&str, &[u8])] = &[
    (
        "01-introduction.txt",
        include_bytes!("../assets/website/01-introduction.txt"),
    ),
    (
        "02-features.txt",
        include_bytes!("../assets/website/02-features.txt"),
    ),
    (
        "03-capabilities.txt",
        include_bytes!("../assets/website/03-capabilities.txt"),
    ),
    (
        "10-banner.png",
        include_bytes!("../assets/website/10-banner.png"),
    ),
    (
        "11-architecture.png",
        include_bytes!("../assets/website/11-architecture.png"),
    ),
    (
        "04-memory.txt",
        include_bytes!("../assets/website/04-memory.txt"),
    ),
    (
        "05-filesystem.txt",
        include_bytes!("../assets/website/05-filesystem.txt"),
    ),
    (
        "06-drivers.txt",
        include_bytes!("../assets/website/06-drivers.txt"),
    ),
    (
        "12-filesystem.png",
        include_bytes!("../assets/website/12-filesystem.png"),
    ),
    (
        "13-platform.png",
        include_bytes!("../assets/website/13-platform.png"),
    ),
    ("README.txt", include_bytes!("../assets/website/README.txt")),
];

pub fn is_internal_file(directory: &str, name: &str) -> bool {
    directory == ROOT && name == ".installed-v2.txt"
}

/// Friendly labels affect only this sample directory, never the actual SD file paths.
pub fn display_name<'a>(directory: &str, name: &'a str) -> &'a str {
    if directory != ROOT {
        return name;
    }
    match name {
        "10-banner.png" => "蓝河内核",
        "11-architecture.png" => "内核核心",
        "12-filesystem.png" => "文件系统",
        "13-platform.png" => "平台与芯片架构",
        _ => ASSETS
            .iter()
            .find(|(asset_name, _)| *asset_name == name && name.ends_with(".txt"))
            .and_then(|(_, contents)| std::str::from_utf8(contents).ok())
            .and_then(|text| text.lines().next())
            .unwrap_or(name),
    }
}

fn metadata_if_present(path: &str) -> Result<Option<fs::Metadata>> {
    // The current BlueOS stat() wrapper loses errno when the path is absent. Enumerate the
    // existing parent to distinguish absence from real I/O errors without ignoring either.
    let (parent, name) = path
        .rsplit_once('/')
        .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "expected an absolute SD path"))?;
    for entry in fs::read_dir(parent)? {
        let entry = entry?;
        if entry.file_name() == name {
            return entry.metadata().map(Some);
        }
    }
    Ok(None)
}

/// Compare by streaming through a 1 KiB scratch buffer, never allocating an entire image.
fn matches_file(path: &str, expected: &[u8]) -> Result<bool> {
    let metadata = match metadata_if_present(path)? {
        Some(metadata) => metadata,
        None => return Ok(false),
    };
    if !metadata.is_file() || metadata.len() != expected.len() as u64 {
        return Ok(false);
    }
    let mut file = File::open(path)?;
    let mut scratch = [0u8; 1024];
    for chunk in expected.chunks(scratch.len()) {
        let destination = &mut scratch[..chunk.len()];
        file.read_exact(destination)?;
        if destination != chunk {
            return Ok(false);
        }
    }
    Ok(true)
}

fn install_file(path: &str, contents: &[u8]) -> Result<()> {
    if matches_file(path, contents)? {
        println!(
            "[SDCARD-CONTENT] verified {path} ({} bytes)",
            contents.len()
        );
        return Ok(());
    }

    {
        let mut file = File::create(path)?;
        let mut scratch = [0u8; 1024];
        // Copy small chunks from Flash into SRAM so the SD transport can use ordinary RAM
        // buffers. The embedded asset itself remains in the Flash-mapped read-only segment.
        for chunk in contents.chunks(scratch.len()) {
            scratch[..chunk.len()].copy_from_slice(chunk);
            file.write_all(&scratch[..chunk.len()])?;
        }
        file.flush()?;
    }
    if !matches_file(path, contents)? {
        return Err(Error::new(
            ErrorKind::InvalidData,
            "SD content readback mismatch",
        ));
    }
    println!(
        "[SDCARD-CONTENT] installed and verified {path} ({} bytes)",
        contents.len()
    );
    Ok(())
}

fn remove_legacy_samples() -> Result<()> {
    if matches_file(CLEANUP_MARKER, CLEANUP_RECORD)? {
        return Ok(());
    }
    // Check every target before deleting anything. A reset halfway through cleanup is safe:
    // absent files are skipped, and the marker is written only after all removals succeed.
    for &(path, expected_size) in LEGACY_SAMPLES {
        match metadata_if_present(path)? {
            Some(metadata) if metadata.is_file() && metadata.len() == expected_size => {}
            Some(_) => {
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    "legacy SD sample changed",
                ))
            }
            None => {}
        }
    }
    for &(path, _) in LEGACY_SAMPLES {
        if metadata_if_present(path)?.is_none() {
            continue;
        }
        fs::remove_file(path)?;
        if metadata_if_present(path)?.is_some() {
            return Err(Error::new(
                ErrorKind::Other,
                "removed SD file is still present",
            ));
        }
        println!("[SDCARD-CONTENT] removed {path}");
    }
    install_file(CLEANUP_MARKER, CLEANUP_RECORD)
}

fn audit_directory(path: &str, depth: usize, counts: &mut [usize; 3]) -> Result<()> {
    if depth > 32 {
        return Err(Error::new(
            ErrorKind::InvalidData,
            "SD directory nesting is too deep",
        ));
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        let path = entry.path();
        if metadata.is_dir() {
            audit_directory(
                path.to_str()
                    .ok_or_else(|| Error::new(ErrorKind::InvalidData, "non-UTF8 SD path"))?,
                depth + 1,
                counts,
            )?;
        } else {
            match path.extension().and_then(|extension| extension.to_str()) {
                Some(extension) if extension.eq_ignore_ascii_case("txt") => counts[0] += 1,
                Some(extension) if extension.eq_ignore_ascii_case("png") => counts[1] += 1,
                _ => {
                    counts[2] += 1;
                    println!("[SDCARD-CONTENT] other file retained: {}", path.display());
                }
            }
        }
    }
    Ok(())
}

pub fn install() -> Result<String> {
    fs::create_dir_all(ROOT)?;
    for &(name, contents) in ASSETS {
        let path = format!("{ROOT}/{name}");
        install_file(&path, contents)
            .map_err(|error| Error::new(error.kind(), format!("install {path}: {error}")))?;
    }
    remove_legacy_samples()?;
    let mut counts = [0; 3];
    audit_directory("/data", 0, &mut counts)?;
    println!(
        "[SDCARD-CONTENT] SD audit: {} TXT, {} PNG, {} other files",
        counts[0], counts[1], counts[2]
    );
    Ok(ROOT.to_string())
}
