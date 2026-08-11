# Ranger

A portable file browser for Windows and Linux. Single executable, no installer, no administrator rights, no Internet.

Ranger came out of a simple frustration: wanting several folders open side by side, being able to come back to the same arrangement tomorrow, and not wondering what the program does when nobody is looking. It is written in Rust with [Slint](https://slint.dev), which is why it starts fast and stays small.
I am Ranger's first user, and I use it daily literally every day.

Copy the executable where you want and run it. That is the whole installation.

## Private by construction

Ranger makes no network request. There is no account, no telemetry, no update check, no crash reporting.

Its settings, favourites and workspaces are plain TOML files in the usual per-user folders. You do not have to guess where: **Settings → Storage locations** lists the exact paths and opens them.

Thumbnails live in a bounded memory cache for the length of the session. Nothing is written to a thumbnail database on disk.

Ranger runs with ordinary user permissions. It installs no service and no background process. It still obeys the permissions of the files you ask it to touch, obviously.

## What it does

**Panels.** Split the window horizontally or vertically, up to 16 panels. Each keeps its own tabs, history, sorting and columns.

**Workspaces.** A workspace is a saved set of panels and tabs. Keep one per project and restore the whole arrangement in one click.

**Favourites.** Shared across every workspace, so your usual places do not depend on which layout is open.

**Custom commands.** Run any program on the selection with your own arguments, using placeholders: `{file}`, `{files}` (the whole selection), `{dir}`, `{dirname}`, `{name}`, `{stem}`, `{ext}`, `{names}`, `{uri}`. A command can be pinned to the right-click menu, and restricted to the extensions where it makes sense.

Ready-made recipes fill the editor for you when the tool is installed — for instance **Compress to zip** or **Extract to folder**, with an icon identifying the tool. This is genuinely useful on Linux, where no equivalent shell integration exists. The editor shows the exact command line and whether it will run once or once per selected file, before you save anything.

**Keyboard.** 39 actions follow the usual conventions and can all be rebound from Settings.

**Columns.** Show, hide, resize and reorder them per panel.

**Filtering.** Start typing to narrow a folder by name, or open the extension filter for something stricter.

**Recursive folder size and date.** Off by default. Turn either on and choose a depth from 1 to 8: Ranger then shows the real size and latest modification date of a folder's contents, computed in the background. Both metrics share a single directory walk, so enabling the two costs one pass, not two.

**Files.** Copy, move, link, rename, create, trash and delete, with progress reported without freezing the window. Several operations can run at once. Multi-selection, drag and drop between panels and to other applications, and reopening closed tabs.

**Interface.** English, French, Spanish, German and Italian. Light and dark themes. Adjustable scaling.

## Previews

Images are decoded in pure Rust, no C library and no network:

`.jpg` `.jpeg` · `.png` · `.gif` · `.webp` · `.bmp` · `.tif` `.tiff` · `.tga` · `.hdr` · `.ico`

`.svg` is rendered by Slint's own vector engine.

PSD files (`.psd`) display Photoshop's embedded JPEG thumbnail when one is
present. This is deliberately best-effort.

Affinity Photo, Designer and Publisher files (`.afphoto`, `.afdesign`, `.afpub`
and `.af`) receive the same lightweight treatment.

MP3 and FLAC files (`.mp3`, `.flac`) display their embedded cover art. A track without embedded artwork simply keeps the normal audio icon.

Videos (`.mp4` `.mkv` `.mov` `.avi` `.webm` `.wmv` `.flv` `.m4v` `.mpg` `.mpeg` `.ts` `.3gp`) and PDFs get a preview too, through a different route on each system:

- **Windows** — the shell's thumbnail providers, so nothing extra to install. Formats a codec pack has taught the system about work as well.
- **Linux** — `ffmpeg` for video, Poppler (`pdftoppm` or `pdftocairo`) for PDF. Both optional; without them Ranger simply shows the file-type icon.

Formats outside this list — RAW, and sometimes HEIF or AVIF — are recognised and given their type icon, but not decoded into a thumbnail.

## About AI-assisted development

Ranger is built with AI assistance. Saying so plainly seems better than leaving it to be discovered.

Nothing was accepted because it compiled. Every change was carefully tried by hand on both Windows and Linux, lived with, and reworked until it held up in the awkward cases as well as the obvious ones. That patient testing and finishing took far longer than the writing — it is most of what this project actually is. 

## Building

Needs Rust 1.88 or newer and a C compiler. The first release build takes several minutes: it is optimised with link-time optimisation.

### Windows

Install Rust from <https://rust-lang.org/tools/install/> (the x64 `rustup-init.exe`), then the Visual Studio Build Tools with the **Desktop development with C++** workload, which provides MSVC and the Windows SDK:

```powershell
winget install Microsoft.VisualStudio.2022.BuildTools --override "--wait --passive --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
```

Nothing else is needed — previews go through the system.

### Linux

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Then the build dependencies:

- Debian / Ubuntu / Mint — `sudo apt install build-essential pkg-config libfontconfig1-dev`
- Fedora — `sudo dnf install gcc pkgconf-pkg-config fontconfig-devel`
- Arch — `sudo pacman -S --needed base-devel fontconfig`

Optionally `ffmpeg` for video previews and `poppler-utils` for PDF previews.

### Build and run

```sh
cargo run --release --bin ranger
```

The executable lands in `target/release/` (`ranger` on Linux, `ranger.exe` on Windows). Copy it anywhere. `cargo clean` reclaims the build directory, which is large.

## License

GNU General Public License v3.0 or later.
