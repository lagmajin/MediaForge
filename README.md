# MediaForge CLI

Fast, local-first command-line utilities for game development and video production. The native Rust CLI scans a project or media folder and reports asset counts, storage use, largest files, and same-size duplicate candidates. Core scans make no AI or network calls. A Python implementation of `scan` is kept as a reference while evaluating Rust.

## Requirements

- Rust/Cargo stable for the native CLI
- Python 3.10 or newer only for the reference implementation
- Windows, macOS, or Linux

Run `mediaforge doctor` to check whether `ffmpeg` and `ffprobe` are available on `PATH`, plus the H.264/AAC, MP3, WAV, and FLAC encoders used by the media commands; `--json` emits the result as one object.

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
mediaforge probe ./Media/shot.mp4 --summary
mediaforge probe-dir ./Media --extension mov,mxf --jobs 4
mediaforge probe-dir ./Media --summary --jobs 4
mediaforge probe-dir ./Media --type video --summary --jobs 4
mediaforge probe-dir ./Media --type video --csv --jobs 4
mediaforge probe-dir ./Media --type video --min-size 1G --summary --jobs 4
mediaforge probe-dir ./Media --type video --compare --jobs 4
mediaforge storyboard ./Media/shot.mp4 --output ./Media/shot-sheet.jpg
mediaforge storyboard ./Media/shot.mp4 -o ./Media/shot-sheet.png --interval 5 --columns 6 --rows 4 --width 240
mediaforge scene-detect ./Media/shot.mp4 --threshold 0.4 --min-gap 1 --format jsonl
mediaforge scene-detect ./Media/shot.mp4 --segments --format csv
mediaforge scene-detect ./Media/shot.mp4 --fps 30000/1001
mediaforge scene-detect-dir ./Media --segments --jobs 4
mediaforge proxy ./Media/shot.mp4 --output ./Proxies/shot-proxy.mp4
mediaforge proxy-config init --profile fast --max-width 960 --crf 30 --preset ultrafast
mediaforge proxy ./Media/shot.mp4 -o ./Proxies/shot-proxy.mp4 --profile fast
mediaforge proxy ./Media/shot.mp4 -o ./Proxies/shot-proxy.mp4 --settings ./edit-proxy.conf
mediaforge proxy-dir ./Media --output-dir ./Proxies --extension mov,mxf --max-width 1280 --preset veryfast --jobs 4 --skip-existing
mediaforge proxy-dir ./Media --output-dir ./Proxies --extension mov,mxf --json
mediaforge extract-audio ./Media/shot.mp4 --output ./Audio/shot.mp3
mediaforge extract-audio ./Media/shot.mp4 -o ./Audio/shot.wav --format wav --stream 1
mediaforge extract-audio-dir ./Media --output-dir ./Audio --extension mov,mxf --format mp3 --bitrate 192k --jobs 4 --skip-existing --json
```

`probe` invokes `ffprobe` directly (without a shell) and returns its JSON containing container and stream metadata such as duration, codec, dimensions, frame rate, and audio channels. Use `--summary` to return only the compact comparison fields: file size, duration, bitrate, codecs, dimensions, pixel and aspect formats, frame rates, and audio layout. `--compact` removes JSON whitespace. `probe-dir` scans video/audio assets and probes them concurrently, emitting one JSON Lines record per file in stable path order. It can filter by extension, `--type video|audio`, or `--min-size`/`--max-size` before launching `ffprobe`; size bounds accept bytes or binary K/M/G/T units. The type filter also selects only matching streams in each report, which makes reports smaller when only picture or sound specs matter. `--jobs` controls concurrency from 1 to 32. A failed file is reported as an error record while other files continue. `probe-dir --summary` uses the same compact fields. Add `--csv` for a spreadsheet-friendly row per file with container, duration, first-stream codec/video/audio fields, file size, and bitrate; failures are included with `ok=false` and an error message. `--compare` implies `--summary` and reports only differences in container, codec, profile, dimensions, frame rate, pixel format, sample rate, or audio layout across files, grouping paths that share a value; combine it with `--type video|audio` to compare that stream type consistently. `--compare` and `--csv` are separate output modes. `ffprobe` must be available on `PATH`.

`storyboard` creates a contact sheet using `ffmpeg` (also required on `PATH`). It samples at the requested interval (10 seconds by default), with a 5×5 grid of 320-pixel-wide thumbnails by default. JPG, PNG, and WebP outputs are supported. It refuses to overwrite existing files and only publishes the image after FFmpeg finishes successfully.

`scene-detect` scans the first video stream for likely scene changes using FFmpeg's scene score, returning cut timecodes, frame indices, and scores without rendering a new video. The default threshold is 0.4 on the 0–1 score scale; `--min-gap` defaults to 1 second and keeps the strongest nearby cut. Output defaults to JSON Lines with a final summary; use `--format csv` for a header and one row per detected cut. Frame indices start at zero and are estimated from the probed average frame rate. `--fps` overrides that rate and accepts decimal or rational values such as `30000/1001`. Variable-frame-rate footage can make these indices approximate; the existing `HH:MM:SS.mmm` value remains elapsed-time timecode, not SMPTE drop-frame timecode. Add `--segments` to convert cuts into contiguous intervals from 0 through the probed video duration; interval rows contain start, end, and duration timecodes/seconds plus frame indices for timeline import.

`scene-detect-dir` applies the same detection to every recognized video below a folder, running up to 4 files concurrently by default. It emits each file's scene or segment records in stable path order, then one batch summary JSON record. Use `--jobs` to select 1–16 workers, `--extension` to narrow the input set, and `--include-hidden` to include hidden folders/files. Failed files produce JSON error records while the remaining videos continue; the command exits with an error status if any failed.

`proxy` creates an H.264/AAC MP4 suitable for smoother editing. `--profile fast` uses 960 pixels, CRF 32, and `ultrafast`; `balanced` uses 1280, CRF 28, and `veryfast`; `quality` uses 1920, CRF 22, and `fast`. `balanced` is the default. Explicit `--max-width`, `--crf` (0–51), and `--preset` values override the selected profile. Lower CRF preserves more quality; faster presets trade compression efficiency for shorter encoding time. FFmpeg must include libx264 and AAC support. Existing files are never overwritten.

Both proxy commands automatically read `.mediaforge-proxy` from the current working directory when present. Create it with `mediaforge proxy-config init`; choose a profile and optional overrides when initializing. The command refuses to overwrite an existing file. Use `--settings FILE` to select another file. The plain `key=value` format supports `profile`, `max_width`, `crf`, and `preset`; command-line options override file values. For example:

```ini
# .mediaforge-proxy
profile=balanced
max_width=960
crf=30
preset=ultrafast
```

`proxy-dir` applies the same proxy settings to video files in a directory tree, preserving relative folders beneath an existing output directory. It skips files already inside that output directory, never overwrites existing proxies, continues after per-file errors, and returns a nonzero exit status if any file failed. It runs up to 4 `ffmpeg` processes by default; `--jobs` accepts 1–16. Add `--skip-existing` to resume a batch by skipping existing regular output files; other occupied output paths are reported as errors. Skipped files are counted separately. Set `--extension` to limit inputs. Add `--json` for completion-count progress, per-file success/error records in input order, and a final summary as JSON Lines.

`extract-audio` writes the selected audio stream as MP3 (192k by default), WAV, or FLAC. `--stream` selects a zero-based audio stream; `--bitrate` selects an MP3 bitrate from 64k to 320k. The output extension must match `--format`. FFmpeg must include the selected encoder, and existing files are never overwritten.

`extract-audio-dir` applies those settings to video/audio files in a directory tree and preserves relative folders beneath an existing output directory. It skips inputs already inside that output directory, refuses collisions, and continues after per-file errors. It runs up to 4 `ffmpeg` processes by default; `--jobs` accepts 1–16. Add `--skip-existing` to resume a batch by skipping existing regular output files; other occupied output paths are reported as errors. Skipped files are counted separately. Add `--json` for completion-count progress, per-file results in input order, and a final summary as JSON Lines.

To run the Python reference implementation during comparison:

```sh
python -m pip install -e .
python -m mediaforge.cli scan ./Media
```

## Usage

```sh
mediaforge scan ./Media
mediaforge scan-config init
mediaforge scan-config show --json
mediaforge scan ./Game/Content --settings ./.mediaforge-scan --show-filters
mediaforge scan ./Game/Content --settings ./.mediaforge-scan --json
mediaforge scan ./Media --type video --csv
mediaforge scan . --include-hidden
mediaforge scan ./Media --type video --min-size 500M
mediaforge scan ./Game --type project --max-size 1M
mediaforge scan ./Game --path-contains environment --path-contains forest --exclude-path-contains proxy --modified-within 30d --csv --sort modified
mediaforge scan ./Game --strict --json
mediaforge scan ./Media --type video --largest 50 --json
mediaforge scan ./Game --csv --sort size --type texture
mediaforge scan ./Game --csv --csv-info --sort size --type texture 2>scan-info.jsonl
mediaforge scan ./Media --extension exr --extension tif
mediaforge dupes ./Game/Content --include-hidden --min-size 1M
mediaforge dupes ./Media --type audio --extension wav,aiff
mediaforge project ./Game
mediaforge gitignore ./Game
mediaforge organize ./Media --by type
mediaforge pack ./Game --output ../Game.tar --dry-run --json
```

Example .mediaforge-scan for recent source footage:

```ini
type=video
extension=mov,mxf
modified_within=30d
exclude_path_contains=proxy
```

Run it with mediaforge scan ./Game --settings ./.mediaforge-scan --csv --sort modified.

Dot-prefixed hidden files/folders and common generated/dependency directories (`.git`, `node_modules`, virtual environments) are skipped by default; on Windows, the hidden file attribute is honored too. Generated directory-name matching is case-insensitive across platforms. Root and nested `.gitignore` rules are applied during traversal, so ignored directories are pruned. Symlinks are not followed. `scan` and `dupes` accept the same asset `--type` categories and a case-insensitive, comma-separated `--extension` list; use `none` to select files without an extension. Wwise `.wem`/`.bnk` and FMOD `.bank`/`.fsb` files are classified as audio; Wwise `.wproj`/`.wwu` and FMOD `.fspro` files are project assets; Unreal `.pak`/`.utoc`/`.ucas` and Godot `.pck` packages are classified as project assets. For `scan`, repeat `--extension` to combine extension groups; matches use OR semantics and JSON reports the deduplicated sorted set. `scan --modified-since` and `--modified-until` accept inclusive Unix-second boundaries and exclude files whose modification time is unavailable. `--modified-within` accepts positive integer durations such as `30m`, `12h`, `7d`, or `2w`; it records both the duration and the resolved Unix-second cutoff. It cannot be combined with the absolute-time filters. `--path-contains TEXT` selects relative paths containing the given case-insensitive substring, useful for narrowing a scan to a folder or asset-name pattern. Repeat the option to require multiple substrings in the same path. Path, type, extension, size, and modification-time filters are applied during traversal where possible, reducing metadata reads and retained records for narrow scans. `--exclude-path-contains` removes paths matching any repeated case-insensitive substring, such as proxy renders or temporary exports; matching directories are pruned during traversal, avoiding work on their contents. JSON records both inclusion and exclusion conditions as arrays. `scan --largest N` controls the number of largest files shown in text or returned in JSON; defaults are 10 in text and 20 in JSON. Ties are ordered by case-insensitive relative path, then exact path, for repeatable output. `scan --csv` emits one row per match with `path`, `type`, `extension`, `size_bytes`, and `modified_unix_seconds`, including all matches regardless of `--largest`. It can be piped to spreadsheet or shell tools without the summary formatting. CSV returns every match regardless of `--largest`; `scan --show-filters` marks `largest_limit_applies` false for CSV output and reports the limit source as `explicit` or `default` along with the format default. The `project` type includes engine assets such as Unity prefabs/assets, Unreal `.uasset` files, and Godot scenes/resources. The duplicate section only groups equal file sizes; it does not claim the files have identical content. JSON output includes file counts, byte totals by type and extension, and relative paths for scripts and other tools. The best-effort `scan` skips filesystem paths it cannot inspect; text and JSON mark incomplete results, while CSV sends a warning to stderr (`incomplete`, `skipped_paths`, up to 100 `skipped_path_examples`, and `skipped_paths_truncated` are available in JSON). Use `scan --strict` when the same scan filters and output formats should fail on the first traversal error instead of reporting partial results; `dupes`, `pack`, and `snapshot` are also strict.

`--min-size` and `--max-size` accept byte values and binary size units (`K`/`KiB`, `M`/`MiB`, `G`/`GiB`, `T`/`TiB`), including decimal fractions. Both byte bounds are inclusive after fractional thresholds are rounded up to the next whole byte; `--max-size` must be greater than or equal to `--min-size`. `scan --csv --sort path` sorts all CSV rows by relative path; `--sort size` orders them largest first; `--sort modified` orders newest first, places unavailable timestamps last, and uses deterministic path tie-breaking. `--sort` is only valid with `--csv`. Add `--csv-info` to write one JSON scan-info record to stderr with row count, applied filters, sort order, and incomplete-scan details; CSV stdout keeps its normal header and rows, so callers can redirect the metadata stream separately. `--csv-info` requires `--csv`. `scan --json` records `schema_version: 1` plus the applied type, size, modification-time, relative-path substring, extension, hidden-file, and strict-mode filters in a `filters` object so saved reports are self-describing. Add `--no-same-size` to omit the potentially large `same_size_candidates` path list while preserving its group count; JSON sets the field to `null` and `same_size_candidates_included` to `false`. This is useful when an AI or script only needs the summary and largest files. Use `mediaforge scan-config init` to create a reusable `.mediaforge-scan` example, then run `mediaforge scan-config validate` to check its keys, values, and cross-field constraints without scanning files. Use `mediaforge scan-config show` to inspect active `key=value` filters from the file; add `--json` for a machine-readable array that preserves repeated keys and their order. Add `scan --show-filters` to preview the resolved JSON filters after command-line overrides; it exits before inspecting the target directory. Pass a validated file with `scan --settings FILE`. The file uses `key=value` lines and supports `type`, `min_size`, `max_size`, `modified_since`, `modified_until`, `modified_within`, repeated `extension`, `path_contains`, `exclude_path_contains`, `include_hidden`, `strict`, and `largest` (which limits text/JSON results but not CSV rows). Scalar command-line options override file values; repeated path and extension filters combine with the configured values. The initializer refuses to overwrite an existing file. Repeated `extension`, `path_contains`, and `exclude_path_contains` keys are allowed; duplicate scalar keys such as `type` are rejected with a line number.

## Find exact duplicates

`dupes` groups candidates by file size, compares small samples from the beginning, middle, and end to skip obvious non-matches, then verifies remaining candidates byte-for-byte in streaming chunks. It reports reclaimable space without loading entire video files into memory. Sample reads run concurrently; `--jobs N` controls 1–64 workers and defaults to up to four (use `--jobs 1` for a busy or slower disk). `--min-size` and `--max-size` can bound the sizes examined, helping skip large files that are not relevant to a particular cleanup. `--top-groups N` limits the displayed groups and sorts by reclaimable space; all eligible files are still checked, and summary totals cover every verified group. JSON reports the size bounds and total, returned, and omitted group counts. It never deletes or modifies files. Unlike the summary `scan`, `dupes`, `organize`, and `pack` stop with an error if a directory or file cannot be inspected, so they do not silently present an incomplete result as complete.

```sh
mediaforge dupes ./Media
mediaforge dupes ./Game/Content --json --min-size 1M
mediaforge dupes ./Media --include-hidden
mediaforge dupes ./Media --top-groups 10 --json
mediaforge dupes ./Media --type video --max-size 2G --top-groups 20 --jobs 4
```

## Inspect a project

```sh
mediaforge project ./Game
mediaforge project ./Game --json
mediaforge unity-meta ./Game --json
mediaforge snapshot ./Game --output ./Game.mfs
mediaforge snapshot ./Game --output ./Game-content.mfs --hash
mediaforge snapshot ./Game --diff ./Game.mfs --json --fail-on-change
mediaforge snapshot ./Game --diff ./Game.mfs --csv
mediaforge snapshot ./Game --diff ./Game.mfs --update
```

The read-only inspector recognizes Unity, Unreal Engine, and Godot project markers. It reports missing expected folders and measures the actual contents of known build/cache folders (`Library`, `Intermediate`, `DerivedDataCache`, `.godot`, `.import`, and related folders) without deleting them; `.gitignore` rules do not reduce these size totals, and symlinks are not followed. For Unreal projects, it also reports the top-level `EngineAssociation` value from each `.uproject` descriptor. For Godot, it reads the `[application]` `config/features` array from `project.godot`. Some folders, such as Unreal's `Saved`, can also contain backups or logs. It also lists Blender, DaVinci Resolve, Premiere Pro, After Effects, Final Cut Pro XML, Wwise, FMOD Studio, AAF, EDL, and MLT project files found below the chosen folder; `.gitignore` rules apply, and hidden folders are included unless ignored.

`unity-meta` checks every included file and folder under Unity's `Assets` directory for its matching `.meta` sidecar, reports orphaned metadata, and validates each metadata file's GUID for missing, malformed, or duplicate values. It honors `.gitignore` rules and skips hidden entries by default; `--include-hidden` includes them. The command is read-only, emits JSON with `--json`, and exits with an error status when it finds issues, so it can be used as a local pre-commit check.

`snapshot` stores portable relative paths, file sizes, and modification times in a compact binary manifest, then compares a later scan to report added, removed, and changed files. JSON Lines mark each change with an `asset_type` inferred from its extension and include old/new sizes, size deltas, and modification times; the final summary includes added/removed bytes and per-type counts. `--csv` writes the same change rows and final/type summaries as spreadsheet-ready records. It skips common Unity/Unreal/Godot generated caches by default, honors `.gitignore`, and records its inclusion settings in the manifest. The baseline can live inside the scanned directory; MediaForge excludes that manifest automatically. Metadata-only mode is the default and does not read file contents, so a rewrite that preserves both size and modification time is not detected. Add `--hash` when creating a baseline to store SHA-256 fingerprints; diffs against that baseline stream file contents and detect same-size/same-time rewrites while ignoring timestamp-only changes. Hash mode is stored in the manifest and inherited by `--diff`. Hash workers run concurrently, with `--jobs N` selecting 1–64 workers (default up to four); choose a lower value for a busy or slower disk. Use `--update` with `--diff` to atomically replace that baseline after emitting the diff, avoiding a second project scan. `--update` can also be combined with `--fail-on-change`; the baseline is updated before the command returns a nonzero status for detected changes. Use `--overwrite` to atomically replace a previous baseline when creating one, and `--fail-on-change` to return a nonzero status when a diff contains changes.

## Generate engine-specific Git ignore patterns

```sh
mediaforge gitignore ./Game
mediaforge gitignore ./Game --apply
mediaforge gitignore ./Game --json
mediaforge gitignore ./Game --apply --json
```

`gitignore` detects Unity, Unreal, and Godot project markers and previews the corresponding generated-file patterns. It only appends patterns that are not already present, and only with `--apply`. Add `--json` for a single object containing the detected engines, target path, result (`preview`, `applied`, or `already_current`), and proposed/applied patterns. Existing `.gitignore` files are preserved; symlinks and non-file targets are refused. Unreal's `Saved/` is deliberately left out because it can contain backups and logs.

## Organize files with preview and undo

```sh
mediaforge organize ./Media --by type
mediaforge organize ./Media --by extension
mediaforge organize ./Media --by type --type video --min-size 500M --json
mediaforge organize ./Media --by extension --include-hidden
mediaforge organize ./Media --by size --type video --json
mediaforge organize ./Game --by type --include-generated
mediaforge organize ./Media --by type --apply
mediaforge undo ./Media
mediaforge undo ./Media --json
```

Organization is a preview unless `--apply` is supplied. Use `--type`, `--extension`, and `--min-size` to limit which files are organized; for example, `--type video --min-size 500M` selects large video assets. `--by size` creates five buckets: under 1 MiB, 1–64 MiB, 64–512 MiB, 512 MiB–2 GiB, and 2 GiB or larger. Hidden files are skipped by default; pass `--include-hidden` to include them. Unity, Unreal, and Godot generated folders such as `Library`, `Intermediate`, `DerivedDataCache`, `.godot`, and `.import` are skipped by default; pass `--include-generated` to include them. Add `--json` to emit one machine-readable JSON object with the filtered move plan, counts, and inclusion settings; with `--apply`, the object reports the completed move count and undo manifest path. Moves preserve paths below the new type/extension/size folder, skip destination collisions, and refuse symlink paths. Before moving anything, MediaForge writes a `.mediaforge-undo` recovery manifest with portable `/` separators. `undo` restores files only when the original location is clear; it retains the manifest if a conflict needs manual handling. Add `--json` to get restored and conflict counts plus the manifest-retained state as JSON. Older manifests are still readable; ambiguous old records are handled conservatively and may need manual conflict resolution. It never deletes source files.

## Sequence media filenames

```sh
mediaforge sequence ./Media --prefix shot --extension mov,mxf
mediaforge sequence ./Media --prefix shot --start 100 --padding 5 --extension mov,mxf --apply
mediaforge sequence ./Frames --prefix shot --natural --extension png --apply
mediaforge undo ./Media
```

`sequence` previews an alphabetical renaming plan for regular files directly inside the directory. It preserves extensions, skips hidden files by default, ignores Git-ignored paths, and skips any destination that already exists. JSON preview output includes up to 100 skipped destination examples with a reason (`already_named` or `destination_exists`) and a truncation flag. Use `--natural` to sort digit runs by numeric value, so `frame2.png` comes before `frame10.png`; the default remains alphabetical. Use `--apply` to rename; the shared recovery manifest lets `undo` restore the operation. Only one file operation can be pending per directory so the undo history stays unambiguous.

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
