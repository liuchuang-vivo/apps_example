# BlueOS reading samples

Prepared on 2026-09-08. Attribution is kept in the project, not in on-device TXT files.
The seven Chinese TXT files (six articles and a reading guide) are original explanatory
summaries, not copied articles. Their explanations combine the following primary sources
with the behavior of this board and this example:

- [Official BlueOS kernel site](https://blueos.vivo.com/kernel): positioning and architecture.
- [Official kernel README](https://github.com/vivoblueos/kernel/blob/main/README_zh.md):
  Rust, POSIX, supported architectures, standard library and project organization.
- [Kernel book: system calls](https://github.com/vivoblueos/book/blob/main/src/invoke-syscall.md):
  direct and software-interrupt invocation.
- [Scheduler configuration](https://github.com/vivoblueos/kernel/blob/main/kernel/src/scheduler/Kconfig):
  thread priority and round-robin options, subject to build configuration.
- [Allocator implementations](https://github.com/vivoblueos/kernel/tree/main/allocator/src):
  slab, TLSF and linked-list allocation.
- The local `esp32c6_devkitc_1` board configuration, SD mounting code and this app's
  bounded TXT/PNG readers: `/data`, storage diagnostics, memory costs and display flow.

## Images

Website artwork remains owned by its author. No AI-generated artwork is used.
All four output images are 360 × 326, RGB, non-interlaced PNGs. They fit the existing
streaming decoder and display region without a full-frame RAM allocation.

| SD file | Original artwork | Preparation |
| --- | --- | --- |
| `10-banner.png` | [Official banner](https://blueos.vivo.com/static/img/banner.5bf41761.jpg) | Crop the Rust-themed cube, proportionally resize; enlarge the subject rather than surrounding background |
| `11-architecture.png` | [Official architecture diagram](https://blueos.vivo.com/static/img/arch.09ea9991.jpg) | Extract nine core-service boxes and reflow peer items into two columns; replace the tiny complete-diagram thumbnail |
| `12-filesystem.png` | Same architecture diagram | Extract six filesystem boxes into two rows, redraw the VFS heading at readable size |
| `13-platform.png` | Same architecture diagram | Extract platform startup and architecture boxes; stack/reflow them for larger labels |

These are detail views, not the full architecture or a claim that every depicted feature
is enabled on this board. Box labels retain their original wording. Headings use Noto Sans SC.
The three old safety/lightweight/portability icons are no longer installed.

## Font and pagination

`website-text.ttf` is a regular-weight subset of
[Noto Sans SC](https://github.com/google/fonts/tree/main/ofl/notosanssc), licensed under
the SIL Open Font License in `FONT-LICENSE.txt`. It contains ASCII and the sample text's
Chinese glyphs, not a general-purpose complete Chinese font. Slint embeds rasterized
glyphs in firmware Flash. The full font is not loaded into RAM or copied onto the card.

Body text and file titles are 18 px; the list has five 48 px rows, and bottom controls
are 60 × 44 px. Pagination uses generated font advances, 336 px of usable line width and
nine lines per page. Slint must not wrap these pre-paginated lines a second time.
The glyph seed and advance table are generated together from the same font and documents.

Download `banner.jpg` and `architecture.jpg` from the image URLs above into a temporary
directory, plus `NotoSansSC[wght].ttf` as `NotoSansSC.ttf` and `OFL.txt` from the font repo.
Run `tools/prepare_website_assets.py <directory>` using Pillow and fonttools. Re-run it
whenever sample text changes. Generated assets are checked in; normal builds are offline.

## SD update and checks

Firmware installs these owned samples with 1 KiB transfer buffers and checks every byte
by reading it back. Matching files are not rewritten. The v2 migration removes only the
inventoried random BIN files, the three retired icon PNGs, and the previous migration
record, with exact path/length guards. `.installed-v2.txt` records completion and is hidden
from this example's listing. Other directories and user TXT/PNG files are preserved.

`apps/example/slint_sdcard:check_text_pages` runs the host-side pagination tests and is
included in `apps/example:check_apps`. It checks page boundaries, mixed-width text, tabs,
controls, sample size limits and font coverage. Build/flash the example separately for
real SD I/O and display testing.

`apps/example/slint_sdcard:check_layout` builds the same generated Slint UI for the host,
using the same scanline software renderer as the board. It checks top header placement,
UP hit-testing, 20 TXT and 20 PNG-viewer return cycles, hidden-browser pixel/input
isolation and restoration of the directory frame. Round-screen PPM snapshots are written
to the target's `layout-check` generation directory. PNG snapshots contain only the
viewer frame: SD decoding/direct GDMA writes still require hardware validation.

All fixed-position containers explicitly specify `x` and `y`. In current Slint syntax,
omitting either can center a container inside its parent: an 86 px header inside the
480 px window would move to y=197 and place its navigation buttons behind the content.
The directory page remains instantiated to preserve row allocations, but is invisible
and cannot receive input while a reader is open.
