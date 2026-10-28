import json
import subprocess
import sys
from pathlib import Path

binary = Path(sys.argv[1]).resolve()
repo = Path(__file__).resolve().parents[1]
sample = Path(__file__).parent / "parity_sample"
rust_report = json.loads(subprocess.check_output([str(binary), "scan", str(sample), "--json"], text=True))
python_report = json.loads(
    subprocess.check_output(
        [sys.executable, "-c", "import json,sys; from pathlib import Path; sys.path.insert(0, sys.argv[1]); from mediaforge.cli import scan; print(json.dumps(scan(Path(sys.argv[2]))))", str(repo / "src"), str(sample)],
        text=True,
    )
)

fields = ("files", "total_bytes", "by_type", "extensions", "largest", "same_size_candidates", "same_size_candidate_groups")
for field in fields:
    if rust_report[field] != python_report[field]:
        raise SystemExit(f"Python/Rust mismatch for {field}: {python_report[field]!r} != {rust_report[field]!r}")
print("Python/Rust parity check passed")
