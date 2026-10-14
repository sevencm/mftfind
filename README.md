# mftfind

Near-instant file search for Windows, in the spirit of [Everything](https://www.voidtools.com/)
— but with a smaller footprint, a plain-text-free index format, and a native Rust engine.

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
- Full size and timestamp columns, read from the index without touching disk
- Per-drive index management, with an optional "force walk" mode
- Bundled CJK font, so Chinese filenames render correctly out of the box

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

## Repository layout

```
src/
  main.rs      CLI entry points and argument parsing
  gui.rs       egui / eframe interface
  build.rs     index construction, path resolution, walk fallback
  mft.rs       FSCTL_ENUM_USN_DATA enumeration
  walk.rs      parallel FindFirstFileExW traversal
  format.rs    binary index format (mmap reader + writer)
  search.rs    tokenizer, matcher, sorting
  win.rs       thin Win32 wrappers (volume handles, error classification)
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
