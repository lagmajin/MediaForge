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

On Windows, run `target\release\mediaforge.exe scan .\Media`. The Python reference covers the basic `scan` command and mirrors its JSON report fields; Rust adds asset-type and size filters plus the project workflows. Rust scans are fully offline once the toolchain is installed. Both skip Git-ignored path entries without glob syntax, plus built-in generated/dependency directories. Full Git ignore glob and negation semantics are not implemented yet.

## Inspect media metadata

Install FFmpeg separately to use `probe`; the core scanner does not need it:

```sh
mediaforge probe ./Media/shot.mp4
mediaforge probe ./Media/shot.mp4 --compact
mediaforge probe-dir ./Media --extension mov,mxf --jobs 4
mediaforge probe-dir ./Media --summary --jobs 4
```

`probe` invokes `ffprobe` directly (without a shell) and returns its JSON containing container and stream metadata such as duration, codec, dimensions, frame rate, and audio channels. `probe-dir` scans video/audio assets and probes them concurrently, emitting one JSON Lines record per file in stable path order. It can filter by extension and limits concurrency with `--jobs`; a failed file is reported as an error record while other files continue. Add `--summary` for concise comparable fields: duration, codec, dimensions, frame rate, sample rate, and channels. `ffprobe` must be available on `PATH`.

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
mediaforge scan ./Media --type video --largest 50 --json
mediaforge scan ./Media --extension exr,tif
mediaforge dupes ./Game/Content --include-hidden --min-size 1M
mediaforge dupes ./Media --type audio --extension wav,aiff
mediaforge project ./Game
mediaforge gitignore ./Game
mediaforge organize ./Media --by type
mediaforge pack ./Game --output ../Game.tar --dry-run --json
```

Hidden folders and common generated/dependency directories (`.git`, `node_modules`, virtual environments) are skipped by default, with directory-name matching that is case-insensitive across platforms. Root and nested `.gitignore` rules are applied during traversal, so ignored directories are pruned. Symlinks are not followed. `scan` and `dupes` accept the same asset `--type` categories and a case-insensitive, comma-separated `--extension` list; use `none` to select files without an extension. `scan --largest N` controls the number of largest files shown in text or returned in JSON; defaults are 10 in text and 20 in JSON. The `project` type includes engine assets such as Unity prefabs/assets, Unreal `.uasset` files, and Godot scenes/resources. The duplicate section only groups equal file sizes; it does not claim the files have identical content. JSON output includes file counts, byte totals by type and extension, and relative paths for scripts and other tools.

`--min-size` accepts byte values and binary size units (`K`/`KiB`, `M`/`MiB`, `G`/`GiB`, `T`/`TiB`), including decimal fractions. Fractional thresholds are rounded up to the next whole byte.

## Find exact duplicates

`dupes` groups candidates by file size, compares small samples from the beginning, middle, and end to skip obvious non-matches, then verifies remaining candidates byte-for-byte in streaming chunks. It reports reclaimable space without loading entire video files into memory. `--min-size` and `--max-size` can bound the sizes examined, helping skip large files that are not relevant to a particular cleanup. `--top-groups N` limits the displayed groups and sorts by reclaimable space; all eligible files are still checked, and summary totals cover every verified group. JSON reports the size bounds and total, returned, and omitted group counts. It never deletes or modifies files. Unlike the summary `scan`, `dupes`, `organize`, and `pack` stop with an error if a directory or file cannot be inspected, so they do not silently present an incomplete result as complete.

```sh
mediaforge dupes ./Media
mediaforge dupes ./Game/Content --json --min-size 1M
mediaforge dupes ./Media --include-hidden
mediaforge dupes ./Media --top-groups 10 --json
mediaforge dupes ./Media --type video --max-size 2G --top-groups 20
```

## Inspect a project

```sh
mediaforge project ./Game
mediaforge project ./Game --json
```

The read-only inspector recognizes Unity, Unreal Engine, and Godot project markers. It reports missing expected folders and measures known build/cache folders (`Library`, `Intermediate`, `DerivedDataCache`, `.godot`, `.import`, and related folders) without deleting them. For Unreal projects, it also reports the top-level `EngineAssociation` value from each `.uproject` descriptor. For Godot, it reads the `[application]` `config/features` array from `project.godot`. Some folders, such as Unreal's `Saved`, can also contain backups or logs. It also lists Blender, DaVinci Resolve, Premiere Pro, After Effects, and Final Cut Pro XML project files found below the chosen folder; `.gitignore` rules apply, and hidden folders are included unless ignored.

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
mediaforge organize ./Media --by type --type video --min-size 500M --json
mediaforge organize ./Media --by extension --include-hidden
mediaforge organize ./Game --by type --include-generated
mediaforge organize ./Media --by type --apply
mediaforge undo ./Media
mediaforge undo ./Media --json
```

Organization is a preview unless `--apply` is supplied. Use `--type`, `--extension`, and `--min-size` to limit which files are organized; for example, `--type video --min-size 500M` selects large video assets. Hidden files are skipped by default; pass `--include-hidden` to include them. Unity, Unreal, and Godot generated folders such as `Library`, `Intermediate`, `DerivedDataCache`, `.godot`, and `.import` are skipped by default; pass `--include-generated` to include them. Add `--json` to emit one machine-readable JSON object with the filtered move plan, counts, and inclusion settings; with `--apply`, the object reports the completed move count and undo manifest path. Moves preserve paths below the new type/extension folder, skip destination collisions, and refuse symlink paths. Before moving anything, MediaForge writes a `.mediaforge-undo` recovery manifest with portable `/` separators. `undo` restores files only when the original location is clear; it retains the manifest if a conflict needs manual handling. Add `--json` to get restored and conflict counts plus the manifest-retained state as JSON. Older manifests are still readable; ambiguous old records are handled conservatively and may need manual conflict resolution. It never deletes source files.

## Pack a project for handoff

```sh
mediaforge pack ./Game --output ../Game.tar --dry-run
mediaforge pack ./Game --output ../Game.tar --json
mediaforge pack ./Game --output ../Game-videos.tar --type video
mediaforge pack ./Game --output ../Game-camera-originals.tar --type video --extension mov,mxf
mediaforge pack ./Game --output ../Game-large-videos.tar --type video --min-size 500M
mediaforge pack ./Game --output ../Game-review-proxies.tar --type video --max-size 100M
mediaforge pack ./Game --output ../Game-assets.tar --type image,model,texture
mediaforge pack ./Game --output ../Game-list.tar --dry-run --json --list
```

`pack` creates an uncompressed tar archive using only Rust's standard library. It streams file contents, preserves relative paths and Unix file permissions, and uses PAX extensions for long Unicode paths and files larger than 8 GiB. The plan reports the exact expected tar size, including headers and 512-byte padding, without reading file contents. Add `--json` for a machine-readable summary with the root, output path, result, file count, input bytes, estimated archive bytes, and actual archive bytes after creation; status is `planned`, `output_exists`, or `created`. Combine it with `--dry-run` to get a plan without writing the archive. Add `--list` to include every selected file's relative path, size, and asset type in text output, or in an `entries` array in JSON; without it, JSON keeps `entries` null and the text plan shows up to 30 paths. Archive entries and the output filename that cannot be used on Windows, including reserved device names and Windows-forbidden characters, are rejected. Entry paths that collide when compared without case or resolve to a file/directory conflict are also rejected for reliable extraction on default Windows filesystems. `--type` can limit the archive to one or more scan types: `video`, `audio`, `image`, `model`, `texture`, `project`, or `other`. `--extension` can further limit it to a comma-separated list such as `mov,mxf`; use `none` for files without an extension. `--min-size` can exclude smaller files, for example `--type video --min-size 500M` to package only larger footage; `--max-size` excludes files above an upper bound. Use both to select a size range, and see both thresholds in JSON output. Hidden files, simple `.gitignore` patterns (including escaped glob characters, escaped trailing spaces, and Git-style `**/` matching), and common Unity/Unreal/Godot generated folders (`Library`, `Intermediate`, `DerivedDataCache`, `.godot`, `.import`, and related folders) are skipped by default. Use `--include-generated` to include those folders. Unreal's `Saved` folder is retained because it can contain backups and logs. Symlinks are never followed. Empty directories are not stored. The output must be outside the input folder and must not already exist. Already-compressed video and image files are copied as-is, so the archive can be large.

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
