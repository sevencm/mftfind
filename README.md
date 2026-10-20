# mftfind

[简体中文](README.zh-CN.md)

Near-instant file search for Windows, in the spirit of [Everything](https://www.voidtools.com/)
— a from-scratch Rust implementation, small enough to read end to end.

**This is not an Everything replacement, and it is not a smaller one.**
Everything's 64-bit portable ZIP is 1.8 MB; this project's release package is
3.0 MB, and its binary is larger than that again. Everything's index files are
binary as well, so the index format is not a differentiator either. What mftfind
offers is the opposite of more: the entire pipeline in ~3,300 lines of Rust —
MFT enumeration, index layout, and query path — small enough to read in an
afternoon and modify without fear.

`mftfind` enumerates the NTFS Master File Table directly through
`FSCTL_ENUM_USN_DATA`, writes a custom binary index, and memory-maps that index at
query time so searches run with **zero deserialization overhead**.

On the test machine the index holds **9,210,000+ records across 7 volumes (672 MB)**
and answers a substring query in **~150 ms** on a warm page cache.

---

## How it works

Two independent indexing paths, selected automatically:

| Path | Requirement | Throughput | Notes |
| --- | --- | --- | --- |
| **MFT enumeration** | Administrator rights | ~1–3 s per million files | Preferred; reads the MFT directly, no directory traversal |
| **Parallel walk** | None | ~4–5 s per million files | Fallback when the MFT handle is denied or the volume is unsupported |

If elevation is missing, the built-in `FindFirstFileExW` walker (parallelized with
`rayon`) takes over so the tool is always usable. The GUI exposes a
**"Restart as Administrator"** button that triggers a UAC prompt and re-launches
the process onto the fast path.

One trade-off worth knowing: **`FSCTL_ENUM_USN_DATA` gives neither file size nor
timestamps.** Its records describe changes rather than files, and Microsoft
documents `TimeStamp`, `Reason` and `SourceInfo` as invalid for the sibling
`FSCTL_READ_FILE_USN_DATA` call; enumeration behaves the same way in practice —
sampling the five MFT-built indexes on this machine, 100% of records carried a
zero timestamp. Everything sidesteps this by reading the `$MFT` byte stream
directly rather than going through the journal API.

So the MFT path stores those fields as *unknown* (`size = u64::MAX`,
`mtime = 0`) and the GUI fills them in per visible row with a single
`fs::metadata` call, cached as you scroll. The walk path has both values from
`FindFirstFileExW` and never touches the disk again. Sorting by size or modified
time is therefore only meaningful on walk-built volumes.

### The index format

A single `.fst` file per volume, designed to be mapped rather than parsed:

```
[0..16)   magic  "FSTSEARCH-INDEX1"
[16..20)  version      u32
[20..24)  record count u32
[24..32)  build time   i64 (unix seconds)
[32..36)  search blob length  (ASCII-lowercased names)
[36..40)  display blob length (original-case names)
[40..44)  record area length  (= count * 28)
[44..64)  reserved
[64..)    records[count], 28 bytes each
            0..4   name_off  u32   offset into both name blobs
            4..6   name_len  u16
            6..8   flags     u16   bit0=directory bit1=reparse point bit2=size known
            8..12  parent    u32   parent record index, u32::MAX = root
           12..20  size      u64   u64::MAX when unknown
           20..28  mtime     i64   unix seconds
          followed by the lowercased blob and the original-case blob
```

Because lowercasing only touches ASCII (byte length is preserved in UTF-8), both
name blobs stay perfectly aligned — one offset per record is enough. Parents are
stored as record indices, so full paths are reconstructed on demand by walking up
the tree instead of being materialized on disk.

## Usage

```
mftfind                              launch the GUI
mftfind gui                          launch the GUI
mftfind index [C D ...] [--walk]     build / refresh indexes (default: all fixed drives)
mftfind search <query> [--limit N]   command-line search
mftfind bench [C ...]                indexing + search benchmark
mftfind drives                       drives, privileges, and index status
mftfind mft [C ...]                  probe MFT enumeration directly (no walk fallback)
```

The `mft` subcommand is the diagnostic entry point: it prints raw Win32 error codes,
so a failed index can be traced to permissions, volume features, or enumeration logic
in one shot.

### Console behavior

Release builds link as `IMAGE_SUBSYSTEM_WINDOWS`
(`#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]` in
`src/main.rs`), so double-clicking the executable does **not** open a console
window. Without that attribute the binary defaults to the console subsystem and
Windows allocates a console on every launch, including from Explorer.

That subsystem has a consequence: the process is created *without* standard
handles, so `println!` would fail and panic. `win::init_console` repairs the
three stream slots before the first print:

- already-valid handles are left untouched, so `mftfind search x > out.txt` and
  pipes keep working;
- for CLI subcommands with an invalid stdout, it attaches to the parent console
  and reopens `CONOUT$`, so output still reaches the terminal that started it;
- for the GUI, or when there is no console at all, the streams are pointed at
  `NUL` — output is discarded rather than crashing the process.

Two details worth knowing: a GUI-subsystem app launched from a command prompt
prints *after* the next prompt, because the shell does not wait for it; and debug
builds keep the console subsystem, so `cargo run -- bench` behaves normally.

### GUI

- Incremental search — results narrow as you type
- Sort by name / modified time / size / kind
- Size and modified-time columns; on MFT-built volumes they are filled in from
  the filesystem for the visible rows as you scroll (see above)
- Open a result, reveal it in Explorer, or copy its full path — four ways in:
  the toolbar button, the right-click menu, `Enter` / `Ctrl+Enter` / `Ctrl+C`,
  or double-clicking the path column
- `↑` / `↓` move the selection without leaving the search box
- Per-drive index management, with an optional "force walk" mode
- Chinese and English UI, switchable from the Language menu
- System font fallback, so Chinese filenames render correctly out of the box

### Languages

The interface ships in Chinese and English. At startup the language comes from
`%APPDATA%\mftfind\ui.ini`, falling back to the system UI language queried via
`GetUserDefaultUILanguage` — Chinese systems get Chinese, everything else gets
English. The **Language menu** in the menu bar switches immediately and
remembers the choice; entries are written in their own language (中文 / English)
so they can be found even when the current UI language is unreadable to you.

Every string lives in `src/i18n.rs` as two `Strings` tables and the UI code
contains no literal labels at all. Adding a language means adding a `Lang`
variant, a table, and one branch each in `strings` / `endonym` / `code` /
`from_code` — nothing else changes. Parameterised messages use `{}` templates so
word-order differences (「命中 3 条」 vs "3 hits") stay inside the tables, and
`cargo test` asserts both tables agree on placeholder counts; a missing
placeholder silently drops a field rather than failing loudly.

Two system fonts are appended to egui's built-in font stack rather than
prepended, so Latin text keeps rendering in egui's own font and the fallbacks
only cover what it lacks: **Segoe UI Symbol** for symbols, then a **CJK font**
(SimHei and friends). The symbol font is not optional — egui 0.36's bundled
fonts cover only `☐ ● • · √ ★ ☆ → ⛃ ☜`, so a menu check mark would render as an
empty box without it. A test renders a frame with the real font stack and
asserts the glyphs are actually present.

The CLI subcommands are still Chinese-only — only the GUI is localized.

## Building

```sh
./build.sh              # release build
./build.sh --debug      # debug build
```

`build.sh` targets the `x86_64-pc-windows-gnu` toolchain and works around two
environment quirks that were measured on the development machine:

1. The `rustc`/`cargo` shims under `~/.cargo/bin` fail silently in this sandbox, so
   the script calls the toolchain binaries by absolute path.
2. Crates such as `windows-targets` use `raw-dylib` on the GNU target and need
   `dlltool.exe` at compile time. It ships with the toolchain under
   `lib/rustlib/x86_64-pc-windows-gnu/bin/self-contained/` and is added to `PATH`
   in POSIX form, because MSYS bash splits on the colon in `C:/...`.

A standard MSVC toolchain works too — plain `cargo build --release` produces a
release binary without the script.

### Notes on dependencies

`eframe` is pulled in with `default-features = false` and only `glow` +
`default_fonts` enabled. The default feature set drags in wayland, X11, and
accesskit, none of which are used on Windows, and trimming it cuts roughly 150
crates from the build graph. `codegen-units = 1` is deliberately *not* set in the
release profile: it applies to all dependencies at that profile and dominates
first-build time for a gain that only shows up in the final binary.

### Icon

The app icon is generated by `tools/make_icon.py` (Pillow) into `assets/`:

- `icon.rgba` — 128×128 raw pixels, compiled into the binary via `include_bytes!`
  and handed to egui with `ViewportBuilder::with_icon`. This is the icon shown in
  the window title bar and taskbar, and it needs no extra dependencies.
- `icon.ico` — multi-size container (16→256), used for the *file* icon that
  Explorer and the Start menu display.

The second one is the awkward part: writing an icon into a PE's `.rsrc` section
normally requires a resource compiler (`rc.exe` on MSVC, `windres` on MinGW), and
GNU `ld` rejects a plain `.res` file outright. `tools/embed_icon.py` therefore
appends the section to the compiled executable by hand — section header,
`SizeOfImage`, `DataDirectory[2]` and the three-level resource directory are all
written from scratch — then re-parses the result from disk to confirm the
resource tree round-trips. The script is idempotent and runs automatically from
`build.sh` and from the release workflow.

Small sizes (≤32 px) drop the lines inside the lens and thicken the strokes;
three thin lines at 16 px turn into a grey smudge.

## Repository layout

```
src/
  main.rs      CLI entry points and argument parsing
  gui.rs       egui / eframe interface
  i18n.rs      UI strings and language selection (Chinese / English)
  build.rs     index construction, path resolution, walk fallback
  mft.rs       FSCTL_ENUM_USN_DATA enumeration
  walk.rs      parallel FindFirstFileExW traversal
  format.rs    binary index format (mmap reader + writer)
  search.rs    tokenizer, matcher, sorting
  win.rs       thin Win32 wrappers (volume handles, console setup, errors)
assets/        generated icons (see "Icon" above)
tools/
  make_icon.py     draws the icon and writes assets/
  embed_icon.py    appends the .rsrc section to a built exe
legacy/
  python-tkinter/   original Python prototype, kept for reference
```

## Requirements

- Windows 10 / 11 with NTFS volumes
- Rust (stable) — GNU or MSVC toolchain
- Administrator rights for the MFT fast path (optional)

## Status

Early but functional. Known open items:

- Incremental search is not yet wired into the GUI event loop beyond the
  debounced re-query; a background index warm-up pass is still pending.
- Cold page cache queries are I/O-bound (~1.4 s for the first scan of 9.2 M
  records); subsequent queries hit ~150 ms.

## Releases

Versions follow [SemVer 2.0.0](https://semver.org/) and are published from
annotated tags on the `vMAJOR.MINOR.PATCH[-CHANNEL.N]` form:

```
v0.1.0               stable
v0.2.0-alpha.1       pre-release, feature set still moving
v0.2.0-beta.1        pre-release, feature set frozen
v0.2.0-rc.1          pre-release, blockers only
```

Pushing such a tag runs `.github/workflows/release.yml`, which validates the tag
against `package.version` in `Cargo.toml`, builds on a Windows runner, and
publishes a GitHub Release with a packaged zip and checksums attached.

To cut one:

```sh
./scripts/release.sh 0.2.0-alpha.1 --push
```

The full rules — which component to bump, what counts as a breaking change
(index format changes in particular), and how CI behaves — are in
[RELEASING.md](RELEASING.md).

## License

Not yet chosen.
