# MediaForge CLI

Fast, local-first command-line utilities for game development and video production. The native Rust CLI scans a project or media folder and reports asset counts, storage use, largest files, and same-size duplicate candidates. Core scans make no AI or network calls. A Python implementation is also kept as an equivalent reference while evaluating Rust.

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

On Windows, run `target\release\mediaforge.exe scan .\Media`. The Python and Rust versions use the same command syntax and report fields. Rust scans are fully offline once the toolchain is installed. Both skip Git-ignored path entries without glob syntax, plus built-in generated/dependency directories. Full Git ignore glob and negation semantics are not implemented yet.

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
```

Hidden folders and common generated/dependency directories (`.git`, `node_modules`, virtual environments) are skipped by default. Symlinks are not followed. The duplicate section only groups equal file sizes; it does not claim the files have identical content. JSON output includes byte counts and relative paths for scripts and other tools.

## Cross-platform roadmap

- [x] Python reference CLI for Windows, macOS, and Linux
- [x] Native Rust CLI with no third-party crate dependencies
- [x] CI build matrix for Windows, macOS, and Linux
- [x] Fast local media and asset inventory with no API/token use
- [ ] Content-verified duplicate detection on demand
- [ ] Optional FFmpeg/ffprobe integration for duration, codecs, frame rate, and dimensions
- [ ] Safe media organization plans with preview and undo
- [ ] Game-engine project checks for Unity, Unreal, and Godot
- [ ] Optional AI assistance only for ambiguous labeling, never required for core commands

## Development

```sh
python -m mediaforge.cli --help
```

MIT licensed. See [LICENSE](LICENSE).
