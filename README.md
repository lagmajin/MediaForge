# MediaForge CLI

Fast, local-first command-line utilities for game development and video production. The first command scans a project or media folder and reports asset counts, storage use, largest files, and same-size duplicate candidates. It uses only the Python standard library and makes no AI or network calls.

## Requirements

- Python 3.10 or newer
- Windows, macOS, or Linux

## Install from a checkout

```sh
python -m pip install .
mediaforge scan ./Media
```

On Windows, use `py -m pip install .` if `python` is not on PATH. The installed `mediaforge` command works from PowerShell, Command Prompt, and POSIX shells.

For development without installing:

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

- [x] Single Python implementation for Windows, macOS, and Linux
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
