# MediaForge CLI

Fast, local-first command-line utilities for game development and video production. The native Rust CLI scans a project or media folder and reports asset counts, storage use, largest files, and same-size duplicate candidates. Core scans make no AI or network calls. A Python implementation of `scan` is kept as a reference while evaluating Rust.

## Requirements

- Rust/Cargo stable for the native CLI
- Python 3.10 or newer only for the reference implementation
- Windows, macOS, or Linux

## Install from a checkout

```sh
cargo install --path .
mediaforge scan ./Media
```

For a portable executable without installing, run `cargo build --release` and use `target/release/mediaforge` (Windows: `target\\release\\mediaforge.exe`).

## Rust native build

The native Rust implementation builds with the stable toolchain and the Rust standard library only:

```sh
cargo build --release
./target/release/mediaforge scan ./Media
```

On Windows, run `target\release\mediaforge.exe scan .\Media`. The Python reference currently covers `scan` and uses the same syntax and report fields for that command; the Rust CLI provides the additional project workflows. Rust scans are fully offline once the toolchain is installed. Both skip Git-ignored path entries without glob syntax, plus built-in generated/dependency directories. Full Git ignore glob and negation semantics are not implemented yet.

## Inspect media metadata

Install FFmpeg separately to use `probe`; the core scanner does not need it:

```sh
mediaforge probe ./Media/shot.mp4
mediaforge probe ./Media/shot.mp4 --compact
```

This invokes `ffprobe` directly (without a shell) and returns its JSON containing container and stream metadata such as duration, codec, dimensions, frame rate, and audio channels. `ffprobe` must be available on `PATH`.

To run the Python reference implementation during comparison:

```sh
python -m pip install -e .
python -m mediaforge.cli scan ./Media
```

## Usage

```sh
mediaforge scan ./Media
mediaforge scan ./Game/Content --json
mediaforge scan . --include-hidden
mediaforge scan ./Media --type video --min-size 500M
mediaforge dupes ./Game/Content --min-size 1M
mediaforge project ./Game
mediaforge gitignore ./Game
mediaforge organize ./Media --by type
mediaforge pack ./Game --output ../Game.tar --dry-run
```

Hidden folders and common generated/dependency directories (`.git`, `node_modules`, virtual environments) are skipped by default. Symlinks are not followed. The duplicate section only groups equal file sizes; it does not claim the files have identical content. JSON output includes byte counts and relative paths for scripts and other tools.

## Find exact duplicates

`dupes` groups candidates by file size, then compares possible matches byte-for-byte in streaming chunks. It reports reclaimable space without loading entire video files into memory. It never deletes or modifies files.

```sh
mediaforge dupes ./Media
mediaforge dupes ./Game/Content --json --min-size 1M
```

## Inspect a project

```sh
mediaforge project ./Game
mediaforge project ./Game --json
```

The read-only inspector recognizes Unity, Unreal Engine, and Godot project markers. It reports missing expected folders and measures known build/cache folders (`Library`, `Intermediate`, `DerivedDataCache`, `.godot`, and related folders) without deleting them. Some folders, such as Unreal's `Saved`, can also contain backups or logs. It also lists Blender, DaVinci Resolve, Premiere Pro, After Effects, and Final Cut Pro XML project files found below the chosen folder.

## Generate engine-specific Git ignore patterns

```sh
mediaforge gitignore ./Game
mediaforge gitignore ./Game --apply
```

`gitignore` detects Unity, Unreal, and Godot project markers and previews the corresponding generated-file patterns. It only appends patterns that are not already present, and only with `--apply`. Existing `.gitignore` files are preserved; symlinks and non-file targets are refused. Unreal's `Saved/` is deliberately left out because it can contain backups and logs.

## Organize files with preview and undo

```sh
mediaforge organize ./Media --by type
mediaforge organize ./Media --by extension
mediaforge organize ./Media --by type --apply
mediaforge undo ./Media
```

Organization is a preview unless `--apply` is supplied. Moves preserve paths below the new type/extension folder, skip destination collisions, and refuse symlink paths. Before moving anything, MediaForge writes a `.mediaforge-undo` recovery manifest with portable `/` separators. `undo` restores files only when the original location is clear; it retains the manifest if a conflict needs manual handling. Older manifests are still readable; ambiguous old records are handled conservatively and may need manual conflict resolution. It never deletes source files.

## Pack a project for handoff

```sh
mediaforge pack ./Game --output ../Game.tar --dry-run
mediaforge pack ./Game --output ../Game.tar
mediaforge pack ./Game --output ../Game-videos.tar --type video
mediaforge pack ./Game --output ../Game-assets.tar --type image,model,texture
```

`pack` creates an uncompressed tar archive using only Rust's standard library. It streams file contents, preserves relative paths and Unix file permissions, and uses PAX extensions for long Unicode paths and files larger than 8 GiB. Paths that cannot be extracted on Windows, including reserved device names and Windows-forbidden characters, are rejected. `--type` can limit the archive to one or more scan types: `video`, `audio`, `image`, `model`, `texture`, `project`, or `other`. Hidden files, simple `.gitignore` patterns, and common Unity/Unreal/Godot generated folders (`Library`, `Intermediate`, `DerivedDataCache`, `.godot`, and related folders) are skipped by default. Use `--include-generated` to include those folders. Unreal's `Saved` folder is retained because it can contain backups and logs. Symlinks are never followed. Empty directories are not stored. The output must be outside the input folder and must not already exist. Already-compressed video and image files are copied as-is, so the archive can be large.

## Cross-platform roadmap

- [x] Python reference CLI for Windows, macOS, and Linux
- [x] Native Rust CLI with no third-party crate dependencies
- [x] Fast local media and asset inventory with no API/token use
- [x] Optional ffprobe metadata inspection
- [x] Filter scan results by asset type and minimum file size
- [x] Content-verified duplicate detection on demand
- [x] Safe media organization plans with preview and undo
- [x] Read-only project and generated-cache inspection for Unity, Unreal, and Godot
- [x] Previewable engine-specific `.gitignore` suggestions
- [x] Streaming tar package creation for project handoff
- [ ] Optional AI assistance only for ambiguous labeling, never required for core commands

## Development (Rust)

```sh
cargo fmt --check
cargo check --offline
cargo build --release --offline
python tools/benchmark_scan.py target/release/mediaforge
```

MIT licensed. See [LICENSE](LICENSE).
