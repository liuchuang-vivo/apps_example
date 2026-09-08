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

//! Bounded pagination for the regular 18 px font in app.slint's 340 x 250 px reader.

mod metrics {
    include!("../assets/text-metrics.rs");
}

const FONT_SIZE: u32 = 18;
// Leave four pixels for glyph rounding/overhang; nine lines fit this font's line height.
const LINE_WIDTH: u32 = 336;
const PAGE_LINES: usize = 9;

fn advance(character: char) -> u32 {
    let units = metrics::ADVANCES
        .binary_search_by_key(&(character as u32), |&(code, _)| code)
        .map(|index| metrics::ADVANCES[index].1)
        .unwrap_or(metrics::UNITS_PER_EM);
    (units * FONT_SIZE).div_ceil(metrics::UNITS_PER_EM)
}

fn finish_line(page: &mut String, pages: &mut Vec<String>, line: &mut usize) {
    if *line + 1 == PAGE_LINES {
        pages.push(std::mem::take(page));
        *line = 0;
    } else {
        page.push('\n');
        *line += 1;
    }
}

pub fn paginate_text(contents: &str) -> Vec<String> {
    // Retain pages only, not a second vector of every line. Newline-heavy 8 KiB input
    // must not create thousands of intermediate String headers on the device heap.
    let mut pages = Vec::new();
    let mut page = String::new();
    let mut line = 0;
    let mut width = 0;
    let mut columns = 0;
    let mut ended_with_newline = false;
    for character in contents.trim_start_matches('\u{feff}').chars() {
        if character == '\r' {
            continue;
        }
        if character == '\n' {
            finish_line(&mut page, &mut pages, &mut line);
            width = 0;
            columns = 0;
            ended_with_newline = true;
            continue;
        }
        ended_with_newline = false;
        let (character, repeats) = match character {
            '\t' => (' ', 4 - columns % 4),
            character if character.is_control() => ('\u{fffd}', 1),
            character => (character, 1),
        };
        for _ in 0..repeats {
            let next_width = advance(character);
            if width != 0 && width + next_width > LINE_WIDTH {
                finish_line(&mut page, &mut pages, &mut line);
                width = 0;
                columns = 0;
            }
            page.push(character);
            width += next_width;
            columns += 1;
        }
    }
    if !page.is_empty() {
        if ended_with_newline {
            page.pop();
        }
        pages.push(page);
    }
    if pages.is_empty() {
        return vec!["(empty file)".to_string()];
    }
    pages
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check_bounds(pages: &[String]) {
        for page in pages {
            assert!(page.split('\n').count() <= PAGE_LINES);
            for line in page.split('\n') {
                assert!(line.chars().map(advance).sum::<u32>() <= LINE_WIDTH);
            }
        }
    }

    #[test]
    fn mixed_text_survives_pagination() {
        let text = "蓝河内核 Rust WWWW iii 480×480！".repeat(120);
        let pages = paginate_text(&text);
        check_bounds(&pages);
        assert!(pages.len() > 1);
        assert_eq!(pages.concat().replace('\n', ""), text);
    }

    #[test]
    fn newlines_tabs_and_controls() {
        assert_eq!(paginate_text("\u{feff}A\tB\r\nC\0"), ["A   B\nC�"]);
        assert_eq!(paginate_text(""), ["(empty file)"]);
        assert_eq!(paginate_text("\r\n"), [""]);
    }

    #[test]
    fn exact_page_boundary() {
        let pages = paginate_text(&"one\n".repeat(PAGE_LINES));
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].split('\n').count(), PAGE_LINES);
        let pages = paginate_text(&format!("{}last", "one\n".repeat(PAGE_LINES)));
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[1], "last");
    }

    #[test]
    fn newline_heavy_file_is_bounded() {
        let pages = paginate_text(&"\n".repeat(8 * 1024));
        check_bounds(&pages);
        assert_eq!(pages.len(), (8usize * 1024).div_ceil(PAGE_LINES));
        assert!(pages.iter().map(String::capacity).sum::<usize>() <= 16 * 1024);
    }

    #[test]
    fn samples_fit_and_have_embedded_glyphs() {
        for contents in [
            include_str!("../assets/website/01-introduction.txt"),
            include_str!("../assets/website/02-features.txt"),
            include_str!("../assets/website/03-capabilities.txt"),
            include_str!("../assets/website/04-memory.txt"),
            include_str!("../assets/website/05-filesystem.txt"),
            include_str!("../assets/website/06-drivers.txt"),
            include_str!("../assets/website/README.txt"),
        ] {
            assert!(contents.len() <= 8 * 1024);
            check_bounds(&paginate_text(contents));
            for character in contents.chars().filter(|character| !character.is_control()) {
                assert!(
                    metrics::ADVANCES
                        .binary_search_by_key(&(character as u32), |&(code, _)| code)
                        .is_ok(),
                    "missing glyph: {character}"
                );
            }
        }
    }
}
