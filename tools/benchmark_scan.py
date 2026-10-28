"""Compare the native Rust scanner and the Python reference on one generated tree."""

from __future__ import annotations

import argparse
import json
import os
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path


def make_dataset(root: Path, file_count: int) -> None:
    suffixes = ("mp4", "wav", "png", "fbx", "bin")
    for index in range(file_count):
        folder = root / f"group-{index % 50:02d}"
        folder.mkdir(exist_ok=True)
        size = 128 + (index % 8) * 64
        contents = bytes([(index * 17 + byte * 29) % 256 for byte in range(size)])
        (folder / f"asset-{index:06d}.{suffixes[index % len(suffixes)]}").write_bytes(contents)


def run_command(command: list[str], cwd: Path, env: dict[str, str]) -> tuple[dict, float]:
    start = time.perf_counter()
    result = subprocess.run(command, cwd=cwd, env=env, text=True, capture_output=True, check=True)
    elapsed = time.perf_counter() - start
    return json.loads(result.stdout), elapsed


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path, help="Path to the compiled mediaforge executable")
    parser.add_argument("--files", type=int, default=5000, help="Number of synthetic files (default: 5000)")
    parser.add_argument("--repeats", type=int, default=7, help="Runs per implementation (default: 7)")
    args = parser.parse_args()
    if args.files < 1 or args.repeats < 1:
        parser.error("--files and --repeats must be positive")

    binary = args.binary.resolve()
    if not binary.is_file():
        parser.error(f"Rust executable not found: {binary}")
    repo = Path(__file__).resolve().parents[1]
    env = os.environ.copy()
    src = str(repo / "src")
    env["PYTHONPATH"] = src + (os.pathsep + env["PYTHONPATH"] if env.get("PYTHONPATH") else "")

    with tempfile.TemporaryDirectory(prefix="mediaforge-bench-") as temp:
        root = Path(temp) / "assets"
        root.mkdir()
        make_dataset(root, args.files)
        commands = {
            "Rust": [str(binary), "scan", str(root), "--json"],
            "Python": [sys.executable, "-m", "mediaforge.cli", "scan", str(root), "--json"],
        }
        reports: dict[str, dict] = {}
        durations: dict[str, list[float]] = {name: [] for name in commands}
        for name, command in commands.items():
            reports[name], _ = run_command(command, repo, env)

        names = list(commands)
        for repeat in range(args.repeats):
            order = names if repeat % 2 == 0 else list(reversed(names))
            for name in order:
                report, elapsed = run_command(commands[name], repo, env)
                reports[name] = report
                durations[name].append(elapsed)

        compared_fields = (
            "files",
            "total_bytes",
            "by_type",
            "extensions",
            "extension_bytes",
            "largest",
            "same_size_candidate_groups",
        )
        for field in compared_fields:
            if reports["Rust"][field] != reports["Python"][field]:
                raise SystemExit(f"Implementations disagree on {field}")

        medians = {name: statistics.median(values) for name, values in durations.items()}
        print(f"Verified identical scan summary: {reports['Rust']['files']} files, {reports['Rust']['total_bytes']} bytes")
        print(f"Warm-up runs: 1 per implementation; measured runs: {args.repeats}; generated files: {args.files}")
        for name in commands:
            values = ", ".join(f"{value * 1000:.2f}" for value in durations[name])
            print(f"{name:>6}: median {medians[name] * 1000:.2f} ms; runs [{values}] ms")
        print(f"Rust/Python median speed ratio: {medians['Python'] / medians['Rust']:.2f}x")
        print("Implementations were warmed once and run in alternating order with a warm filesystem cache; use results as a comparative sample, not a hardware-independent guarantee.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
