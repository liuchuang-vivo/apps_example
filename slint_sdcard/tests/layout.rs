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

//! Exercise the actual generated Slint UI, software scanline renderer and pointer hit tests.
//! Host-only: no SD writes, display hardware, or full-frame allocation in the firmware.

#![feature(cfg_boolean_literals)]

mod app_window {
    include!(env!("SLINT_SDCARD_GENERATED"));
}
#[path = "../src/text_pages.rs"]
mod text_pages;

use app_window::{FileEntry, MainWindow};
use slint::platform::software_renderer::{
    LineBufferProvider, MinimalSoftwareWindow, RepaintBufferType, Rgb565Pixel,
};
use slint::platform::{Platform, PointerEventButton, WindowAdapter, WindowEvent};
use slint::ComponentHandle;
use std::cell::Cell;
use std::io::Write;
use std::path::Path;
use std::rc::Rc;

const WIDTH: usize = 480;
const HEIGHT: usize = 480;

struct Backend(Rc<MinimalSoftwareWindow>);
impl Platform for Backend {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.0.clone())
    }

    fn duration_since_start(&self) -> std::time::Duration {
        std::time::Duration::ZERO
    }
}

struct Frame<'a>(&'a mut [Rgb565Pixel]);
impl LineBufferProvider for Frame<'_> {
    type TargetPixel = Rgb565Pixel;

    fn process_line(
        &mut self,
        line: usize,
        range: std::ops::Range<usize>,
        render_fn: impl FnOnce(&mut [Self::TargetPixel]),
    ) {
        render_fn(&mut self.0[line * WIDTH + range.start..line * WIDTH + range.end]);
    }
}

fn render(window: &MinimalSoftwareWindow) -> Vec<Rgb565Pixel> {
    let mut pixels = vec![Rgb565Pixel(0); WIDTH * HEIGHT];
    window.request_redraw();
    assert!(window.draw_if_needed(|renderer| {
        renderer.render_by_line(Frame(&mut pixels));
    }));
    pixels
}

fn save_frame(path: &Path, pixels: &[Rgb565Pixel]) {
    let mut file = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
    write!(file, "P6\n{WIDTH} {HEIGHT}\n255\n").unwrap();
    for (index, pixel) in pixels.iter().enumerate() {
        // Match the physical round screen when inspecting the host snapshots.
        let x = (index % WIDTH) as i32 - 240;
        let y = (index / WIDTH) as i32 - 240;
        let rgb = if x * x + y * y <= 240 * 240 {
            [
                ((pixel.0 >> 11) * 255 / 31) as u8,
                (((pixel.0 >> 5) & 63) as u16 * 255 / 63) as u8,
                ((pixel.0 & 31) * 255 / 31) as u8,
            ]
        } else {
            [0; 3]
        };
        file.write_all(&rgb).unwrap();
    }
    file.flush().unwrap();
}

fn click(window: &MinimalSoftwareWindow, x: f32, y: f32) {
    let position = slint::LogicalPosition::new(x, y);
    window.dispatch_event(WindowEvent::PointerMoved { position });
    window.dispatch_event(WindowEvent::PointerPressed {
        position,
        button: PointerEventButton::Left,
    });
    window.dispatch_event(WindowEvent::PointerReleased {
        position,
        button: PointerEventButton::Left,
    });
    window.dispatch_event(WindowEvent::PointerExited);
}

fn assert_header(pixels: &[Rgb565Pixel]) {
    // A missing explicit y:0 centers the header at y=197/199, under the content panel.
    let header_rgb565 = ((0x10u16 >> 3) << 11) | ((0x1fu16 >> 2) << 5) | (0x27u16 >> 3);
    assert_eq!(
        pixels[20 * WIDTH + 240].0,
        header_rgb565,
        "header is not at the top"
    );
}

fn main() {
    let output = std::env::args().nth(1).expect("snapshot directory");
    let output = Path::new(&output);
    std::fs::create_dir_all(output).unwrap();
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    window.set_size(slint::PhysicalSize::new(WIDTH as u32, HEIGHT as u32));
    slint::platform::set_platform(Box::new(Backend(window.clone()))).unwrap();
    let ui = MainWindow::new().unwrap();
    let rows: Vec<_> = [
        "认识蓝河内核",
        "内核核心",
        "线程与任务调度",
        "内存与资源预算",
        "从目录到 SD 卡",
    ]
    .into_iter()
    .enumerate()
    .map(|(index, name)| FileEntry {
        name: name.into(),
        is_dir: false,
        is_image: index == 1,
        can_open: true,
        size_text: "1.3 KB".into(),
    })
    .collect();
    let rows = slint::ModelRc::new(slint::VecModel::from(rows));
    ui.set_entries(rows.clone());
    ui.set_total_count(11);
    ui.set_visible_count(5);
    ui.set_path_text("/data/blueos-kernel".into());
    let selections = Rc::new(Cell::new(0));
    let up = Rc::new(Cell::new(0));
    let text_returns = Rc::new(Cell::new(0));
    let image_returns = Rc::new(Cell::new(0));
    let page = text_pages::paginate_text(include_str!("../assets/website/01-introduction.txt"));
    ui.set_text_viewer_content(page[0].as_str().into());
    ui.set_text_viewer_title("认识蓝河内核".into());
    ui.set_text_viewer_page("1 / 4".into());
    ui.set_text_viewer_has_next(true);
    ui.set_image_viewer_title("内核核心".into());
    ui.set_image_viewer_status("360 x 326".into());
    {
        let weak = ui.as_weak();
        let count = selections.clone();
        ui.on_entry_selected(move |index| {
            let ui = weak.upgrade().unwrap();
            count.set(count.get() + 1);
            if index == 1 {
                ui.set_image_viewer_open(true);
            } else {
                ui.set_text_viewer_open(true);
            }
        });
    }
    {
        let count = up.clone();
        ui.on_up_requested(move || count.set(count.get() + 1));
    }
    {
        let weak = ui.as_weak();
        let count = text_returns.clone();
        ui.on_close_text_viewer(move || {
            count.set(count.get() + 1);
            weak.upgrade().unwrap().set_text_viewer_open(false);
        });
    }
    {
        let weak = ui.as_weak();
        let count = image_returns.clone();
        ui.on_close_image_viewer(move || {
            count.set(count.get() + 1);
            weak.upgrade().unwrap().set_image_viewer_open(false);
        });
    }
    ui.show().unwrap();
    let browser = render(&window);
    assert_header(&browser);
    save_frame(&output.join("browser.ppm"), &browser);
    click(&window, 320., 56.);
    assert_eq!(up.get(), 1, "UP button did not receive the click");

    for cycle in 0..20 {
        for image in [false, true] {
            click(&window, 230., if image { 218. } else { 166. });
            assert_eq!(ui.get_image_viewer_open(), image);
            assert_eq!(ui.get_text_viewer_open(), !image);
            let viewer = render(&window);
            assert_header(&viewer);
            if cycle == 0 {
                save_frame(
                    &output.join(if image { "image.ppm" } else { "text.ppm" }),
                    &viewer,
                );
            }

            // Mutating the hidden browser must neither change viewer pixels nor receive input.
            let count = selections.get();
            click(&window, 230., 320.);
            assert_eq!(
                selections.get(),
                count,
                "click leaked to browser below viewer"
            );
            ui.set_entries(slint::ModelRc::default());
            ui.set_total_count(0);
            ui.set_status_text("HIDDEN BROWSER".into());
            let hidden_changed = render(&window);
            assert!(viewer.iter().zip(&hidden_changed).all(|(a, b)| a.0 == b.0));
            ui.set_entries(rows.clone());
            ui.set_total_count(11);

            click(&window, 320., 56.);
            assert!(
                !ui.get_text_viewer_open() && !ui.get_image_viewer_open(),
                "BACK failed"
            );
            let returned = render(&window);
            assert!(
                browser.iter().zip(&returned).all(|(a, b)| a.0 == b.0),
                "viewer left stale pixels"
            );
        }
    }
    assert_eq!(text_returns.get(), 20);
    assert_eq!(image_returns.get(), 20);
    assert_eq!(up.get(), 1, "BACK click leaked to underlying UP");
    println!("UI layout checks passed: top headers, UP, 40 viewer returns, hidden-page isolation, full redraw");
}
