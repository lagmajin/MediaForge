from __future__ import annotations

import argparse
import json
import os
import sys
from collections import Counter, defaultdict
from pathlib import Path
from typing import Iterable

__version__ = "0.1.0"

MEDIA_TYPES = {
    "video": {".mp4", ".mov", ".mxf", ".mkv", ".avi", ".webm", ".m4v"},
    "audio": {".wav", ".aiff", ".aif", ".flac", ".mp3", ".ogg", ".m4a", ".aac"},
    "image": {".png", ".jpg", ".jpeg", ".tif", ".tiff", ".exr", ".hdr", ".tga", ".bmp", ".webp"},
    "model": {".fbx", ".obj", ".gltf", ".glb", ".usd", ".usda", ".usdc", ".blend", ".abc", ".ma", ".mb"},
    "texture": {".sbsar", ".psd", ".psb", ".ktx", ".ktx2", ".dds", ".material"},
    "project": {".uproject", ".unity", ".godot", ".blend", ".drp", ".prproj", ".aep", ".fcpxml"},
}
SKIP_DIRS = {".git", ".hg", ".svn", "node_modules", "__pycache__", ".venv", "venv"}


def classify(path: Path) -> str:
    suffix = path.suffix.lower()
    for kind, extensions in MEDIA_TYPES.items():
        if suffix in extensions:
            return kind
    return "other"


def iter_files(root: Path, include_hidden: bool = False) -> Iterable[Path]:
    """Walk without following symlinks and prune common generated folders."""
    ignored_names: set[str] = set()
    if not include_hidden:
        try:
            for line in (root / ".gitignore").read_text(encoding="utf-8").splitlines():
                pattern = line.strip().lstrip("/").rstrip("/")
                if pattern and not pattern.startswith(("#", "!")) and not any(c in pattern for c in "*?["):
                    ignored_names.add(pattern.replace("\\", "/"))
        except OSError:
            pass

    def onerror(_: OSError) -> None:
        return None

    for current, dirs, files in os.walk(root, followlinks=False, onerror=onerror):
        dirs[:] = sorted(
            d for d in dirs
            if d not in SKIP_DIRS
            and d not in ignored_names
            and (include_hidden or not d.startswith("."))
            and not (Path(current) / d).is_symlink()
        )
        for name in sorted(files):
            path = Path(current) / name
            if not include_hidden and any(part.startswith(".") for part in path.relative_to(root).parts):
                continue
            if not include_hidden and any(part in ignored_names for part in path.relative_to(root).parts):
                continue
            if not path.is_symlink():
                yield path


def human_size(size: int) -> str:
    value = float(size)
    for unit in ("B", "KiB", "MiB", "GiB", "TiB"):
        if value < 1024 or unit == "TiB":
            return f"{value:.1f} {unit}" if unit != "B" else f"{size} B"
        value /= 1024
    return f"{size} B"


def scan(root: Path, include_hidden: bool = False) -> dict:
    root = root.expanduser().resolve()
    if not root.exists():
        raise FileNotFoundError(f"Path does not exist: {root}")
    if not root.is_dir():
        raise NotADirectoryError(f"Expected a directory: {root}")

    counts: Counter[str] = Counter()
    bytes_by_type: Counter[str] = Counter()
    extensions: Counter[str] = Counter()
    largest: list[dict] = []
    by_size: dict[int, list[str]] = defaultdict(list)
    total_bytes = 0
    total_files = 0

    for path in iter_files(root, include_hidden):
        try:
            info = path.stat(follow_symlinks=False)
        except OSError:
            continue
        if not path.is_file():
            continue
        size = info.st_size
        kind = classify(path)
        rel = path.relative_to(root).as_posix()
        total_files += 1
        total_bytes += size
        counts[kind] += 1
        bytes_by_type[kind] += size
        extensions[path.suffix.lower() or "[no extension]"] += 1
        by_size[size].append(rel)
        largest.append({"path": rel, "size_bytes": size, "size": human_size(size), "type": kind})

    largest.sort(key=lambda item: (-item["size_bytes"], item["path"].casefold()))
    duplicate_groups = [
        {"size_bytes": size, "files": sorted(paths, key=str.casefold)}
        for size, paths in sorted(by_size.items()) if len(paths) > 1
    ]
    return {
        "root": str(root),
        "files": total_files,
        "total_bytes": total_bytes,
        "total_size": human_size(total_bytes),
        "by_type": {
            kind: {"files": counts[kind], "bytes": bytes_by_type[kind], "size": human_size(bytes_by_type[kind])}
            for kind in sorted(counts)
        },
        "extensions": dict(sorted(extensions.items(), key=lambda item: (-item[1], item[0]))),
        "largest": largest[:20],
        "same_size_candidates": duplicate_groups,
        "same_size_candidate_groups": len(duplicate_groups),
        "note": "Same-size groups are candidates only; byte-for-byte duplicates are not verified.",
    }


def print_report(report: dict) -> None:
    print(f"MediaForge scan: {report['root']}")
    print(f"{report['files']} files · {report['total_size']}")
    print("\nBy type")
    if not report["by_type"]:
        print("  No files found.")
    for kind, data in report["by_type"].items():
        print(f"  {kind:<9} {data['files']:>6} files  {data['size']:>10}")
    print("\nLargest files")
    for item in report["largest"][:10]:
        print(f"  {item['size']:>10}  {item['path']}")
    print(f"\nSame-size candidate groups: {report['same_size_candidate_groups']} (not content-verified)")


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="mediaforge",
        description="Fast, local-first CLI for game and video production assets.",
    )
    parser.add_argument("--version", action="version", version=f"mediaforge {__version__}")
    sub = parser.add_subparsers(dest="command", required=True)
    scan_parser = sub.add_parser("scan", help="Summarize media and game assets in a folder.")
    scan_parser.add_argument("path", nargs="?", default=".", help="Folder to scan (default: current directory).")
    scan_parser.add_argument("--json", action="store_true", help="Write the full report as JSON.")
    scan_parser.add_argument("--include-hidden", action="store_true", help="Include hidden files and folders.")
    return parser


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        report = scan(Path(args.path), include_hidden=args.include_hidden)
    except (OSError, ValueError) as exc:
        print(f"mediaforge: {exc}", file=sys.stderr)
        return 2
    if args.json:
        json.dump(report, sys.stdout, indent=2, ensure_ascii=False)
        sys.stdout.write("\n")
    else:
        print_report(report)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
