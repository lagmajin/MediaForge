use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::fs;
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const SKIP_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "node_modules",
    "__pycache__",
    ".venv",
    "venv",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Kind {
    Video,
    Audio,
    Image,
    Model,
    Texture,
    Project,
    Other,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Video => "video",
            Self::Audio => "audio",
            Self::Image => "image",
            Self::Model => "model",
            Self::Texture => "texture",
            Self::Project => "project",
            Self::Other => "other",
        }
    }
}

#[derive(Debug)]
struct FileRecord {
    relative: String,
    size: u64,
    kind: Kind,
    extension: String,
}

fn classify(path: &Path) -> Kind {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "mp4" | "mov" | "mxf" | "mkv" | "avi" | "webm" | "m4v" => Kind::Video,
        "wav" | "aiff" | "aif" | "flac" | "mp3" | "ogg" | "m4a" | "aac" => Kind::Audio,
        "png" | "jpg" | "jpeg" | "tif" | "tiff" | "exr" | "hdr" | "tga" | "bmp" | "webp" => {
            Kind::Image
        }
        "fbx" | "obj" | "gltf" | "glb" | "usd" | "usda" | "usdc" | "blend" | "abc" | "ma"
        | "mb" => Kind::Model,
        "sbsar" | "psd" | "psb" | "ktx" | "ktx2" | "dds" | "material" => Kind::Texture,
        "uproject" | "unity" | "godot" | "drp" | "prproj" | "aep" | "fcpxml" => Kind::Project,
        _ => Kind::Other,
    }
}

fn human_size(size: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = size as f64;
    for (index, unit) in UNITS.iter().enumerate() {
        if value < 1024.0 || index == UNITS.len() - 1 {
            return if index == 0 {
                format!("{size} B")
            } else {
                format!("{value:.1} {unit}")
            };
        }
        value /= 1024.0;
    }
    format!("{size} B")
}

fn is_hidden(name: &str) -> bool {
    name.starts_with('.')
}

fn walk(root: &Path, current: &Path, include_hidden: bool, records: &mut Vec<FileRecord>) {
    let Ok(entries) = fs::read_dir(current) else {
        return;
    };
    let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
    entries.sort_by_key(|entry| entry.file_name().to_string_lossy().to_lowercase());
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            if SKIP_DIRS.contains(&name.as_str()) || (!include_hidden && is_hidden(&name)) {
                continue;
            }
            walk(root, &path, include_hidden, records);
        } else if file_type.is_file() && (include_hidden || !is_hidden(&name)) {
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            let relative = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            let extension = path
                .extension()
                .and_then(|s| s.to_str())
                .map(|s| format!(".{}", s.to_ascii_lowercase()))
                .unwrap_or_else(|| "[no extension]".to_owned());
            records.push(FileRecord {
                relative,
                size: metadata.len(),
                kind: classify(&path),
                extension,
            });
        }
    }
}

fn ignored_by_git(root: &Path) -> HashSet<String> {
    let Ok(text) = fs::read_to_string(root.join(".gitignore")) else {
        return HashSet::new();
    };
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#') && !line.starts_with('!'))
        .map(|line| {
            line.trim_start_matches('/')
                .trim_end_matches('/')
                .replace('\\', "/")
        })
        .collect()
}

fn is_ignored(relative: &str, patterns: &HashSet<String>) -> bool {
    patterns.iter().any(|pattern| {
        if pattern.contains('*') || pattern.contains('?') || pattern.contains('[') {
            return false;
        }
        relative == pattern
            || relative.starts_with(&format!("{pattern}/"))
            || relative.split('/').any(|part| part == pattern)
    })
}

fn json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c <= '\u{1f}' => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn print_json(root: &Path, records: &[FileRecord]) {
    let mut by_type: BTreeMap<&str, (u64, u64)> = BTreeMap::new();
    let mut extensions: BTreeMap<&str, usize> = BTreeMap::new();
    let mut by_size: HashMap<u64, Vec<&str>> = HashMap::new();
    let total_bytes: u64 = records.iter().map(|r| r.size).sum();
    for record in records {
        let entry = by_type.entry(record.kind.as_str()).or_default();
        entry.0 += 1;
        entry.1 += record.size;
        *extensions.entry(&record.extension).or_default() += 1;
        by_size
            .entry(record.size)
            .or_default()
            .push(&record.relative);
    }
    let mut largest: Vec<_> = records.iter().collect();
    largest.sort_by(|a, b| {
        b.size
            .cmp(&a.size)
            .then_with(|| a.relative.to_lowercase().cmp(&b.relative.to_lowercase()))
    });
    let mut groups: Vec<_> = by_size
        .into_iter()
        .filter(|(_, files)| files.len() > 1)
        .collect();
    groups.sort_by_key(|(size, _)| *size);
    println!("{{");
    println!("  \"root\": {},", json_string(&root.to_string_lossy()));
    println!("  \"files\": {},", records.len());
    println!("  \"total_bytes\": {},", total_bytes);
    println!(
        "  \"total_size\": {},",
        json_string(&human_size(total_bytes))
    );
    println!("  \"by_type\": {{");
    for (index, (kind, (count, bytes))) in by_type.iter().enumerate() {
        let comma = if index + 1 == by_type.len() { "" } else { "," };
        println!(
            "    {}: {{\"files\": {}, \"bytes\": {}, \"size\": {}}}{}",
            json_string(kind),
            count,
            bytes,
            json_string(&human_size(*bytes)),
            comma
        );
    }
    println!("  }},\n  \"extensions\": {{");
    let mut ext_sorted: Vec<_> = extensions.iter().collect();
    ext_sorted.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    for (index, (extension, count)) in ext_sorted.iter().enumerate() {
        let comma = if index + 1 == ext_sorted.len() {
            ""
        } else {
            ","
        };
        println!("    {}: {}{}", json_string(extension), count, comma);
    }
    println!("  }},\n  \"largest\": [");
    for (index, record) in largest.iter().take(20).enumerate() {
        let comma = if index + 1 == largest.len().min(20) {
            ""
        } else {
            ","
        };
        println!(
            "    {{\"path\": {}, \"size_bytes\": {}, \"size\": {}, \"type\": {}}}{}",
            json_string(&record.relative),
            record.size,
            json_string(&human_size(record.size)),
            json_string(record.kind.as_str()),
            comma
        );
    }
    println!("  ],\n  \"same_size_candidates\": [");
    for (index, (size, files)) in groups.iter().enumerate() {
        let comma = if index + 1 == groups.len() { "" } else { "," };
        let file_values = files
            .iter()
            .map(|file| json_string(file))
            .collect::<Vec<_>>()
            .join(", ");
        println!(
            "    {{\"size_bytes\": {}, \"files\": [{}]}}{}",
            size, file_values, comma
        );
    }
    println!("  ],");
    println!("  \"same_size_candidate_groups\": {},", groups.len());
    println!(
        "  \"note\": \"Same-size groups are candidates only; byte-for-byte duplicates are not verified.\"\n}}"
    );
}

fn print_report(root: &Path, records: &[FileRecord]) {
    let total_bytes: u64 = records.iter().map(|r| r.size).sum();
    let mut by_type: BTreeMap<&str, (u64, u64)> = BTreeMap::new();
    let mut by_size: HashMap<u64, usize> = HashMap::new();
    for record in records {
        let entry = by_type.entry(record.kind.as_str()).or_default();
        entry.0 += 1;
        entry.1 += record.size;
        *by_size.entry(record.size).or_default() += 1;
    }
    let mut largest: Vec<_> = records.iter().collect();
    largest.sort_by(|a, b| {
        b.size
            .cmp(&a.size)
            .then_with(|| a.relative.to_lowercase().cmp(&b.relative.to_lowercase()))
    });
    let candidate_groups = by_size.values().filter(|count| **count > 1).count();
    println!("MediaForge scan: {}", root.display());
    println!("{} files · {}", records.len(), human_size(total_bytes));
    println!("\nBy type");
    if by_type.is_empty() {
        println!("  No files found.");
    }
    for (kind, (count, bytes)) in by_type {
        println!("  {kind:<9} {count:>6} files  {:>10}", human_size(bytes));
    }
    println!("\nLargest files");
    for record in largest.iter().take(10) {
        println!("  {:>10}  {}", human_size(record.size), record.relative);
    }
    println!("\nSame-size candidate groups: {candidate_groups} (not content-verified)");
}

fn files_identical(left: &Path, right: &Path) -> io::Result<bool> {
    let mut left = BufReader::new(fs::File::open(left)?);
    let mut right = BufReader::new(fs::File::open(right)?);
    loop {
        let (left_empty, right_empty, count, equal) = {
            let left_chunk = left.fill_buf()?;
            let right_chunk = right.fill_buf()?;
            let count = left_chunk.len().min(right_chunk.len());
            (
                left_chunk.is_empty(),
                right_chunk.is_empty(),
                count,
                left_chunk[..count] == right_chunk[..count],
            )
        };
        if left_empty || right_empty {
            return Ok(left_empty && right_empty);
        }
        if !equal {
            return Ok(false);
        }
        left.consume(count);
        right.consume(count);
    }
}

fn run_dupes(args: &[String]) -> Result<(), String> {
    let mut json = false;
    let mut include_hidden = false;
    let mut min_size = 0;
    let mut path = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--json" => json = true,
            "--include-hidden" => include_hidden = true,
            "--min-size" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--min-size requires a value".to_owned())?;
                min_size = parse_size(value)?;
            }
            "--help" | "-h" => {
                println!(
                    "Usage: mediaforge dupes [PATH] [--json] [--include-hidden] [--min-size SIZE]\n\nFind byte-for-byte duplicate files. Files are compared in fixed-size chunks to keep memory use low."
                );
                return Ok(());
            }
            value if value.starts_with('-') => return Err(format!("unknown option: {value}")),
            value => {
                if path.is_some() {
                    return Err("only one duplicate scan path may be provided".into());
                }
                path = Some(PathBuf::from(value));
            }
        }
    }
    let path = path.unwrap_or_else(|| PathBuf::from("."));
    let root = fs::canonicalize(&path)
        .map_err(|error| format!("cannot access {}: {error}", path.display()))?;
    if !root.is_dir() {
        return Err(format!("expected a directory: {}", root.display()));
    }
    let mut records = Vec::new();
    walk(&root, &root, include_hidden, &mut records);
    if !include_hidden {
        let ignored = ignored_by_git(&root);
        records.retain(|record| !is_ignored(&record.relative, &ignored));
    }
    let mut by_size: BTreeMap<u64, Vec<String>> = BTreeMap::new();
    for record in records.into_iter().filter(|record| record.size >= min_size) {
        by_size
            .entry(record.size)
            .or_default()
            .push(record.relative);
    }

    let mut groups: Vec<(u64, Vec<String>)> = Vec::new();
    for (size, paths) in by_size.into_iter().filter(|(_, paths)| paths.len() > 1) {
        let mut candidates: Vec<Vec<String>> = Vec::new();
        for relative in paths {
            let candidate = root.join(&relative);
            let mut matched = false;
            for group in &mut candidates {
                let representative = root.join(&group[0]);
                match files_identical(&representative, &candidate) {
                    Ok(true) => {
                        group.push(relative.clone());
                        matched = true;
                        break;
                    }
                    Ok(false) => {}
                    Err(error) => {
                        return Err(format!(
                            "could not compare {}: {error}",
                            candidate.display()
                        ));
                    }
                }
            }
            if !matched {
                candidates.push(vec![relative]);
            }
        }
        for group in candidates.into_iter().filter(|group| group.len() > 1) {
            groups.push((size, group));
        }
    }

    let duplicate_files: usize = groups.iter().map(|(_, files)| files.len()).sum();
    let reclaimable: u64 = groups
        .iter()
        .map(|(size, files)| size.saturating_mul(files.len().saturating_sub(1) as u64))
        .sum();
    if json {
        println!(
            "{{\n  \"root\": {},\n  \"groups\": [",
            json_string(&root.to_string_lossy())
        );
        for (index, (size, files)) in groups.iter().enumerate() {
            let comma = if index + 1 == groups.len() { "" } else { "," };
            let paths = files
                .iter()
                .map(|file| json_string(file))
                .collect::<Vec<_>>()
                .join(", ");
            println!(
                "    {{\"size_bytes\": {}, \"files\": [{}]}}{}",
                size, paths, comma
            );
        }
        println!(
            "  ],\n  \"duplicate_files\": {},\n  \"reclaimable_bytes\": {},\n  \"reclaimable_size\": {}\n}}",
            duplicate_files,
            reclaimable,
            json_string(&human_size(reclaimable))
        );
    } else {
        println!(
            "Verified duplicate files: {duplicate_files} · potential reclaim: {}",
            human_size(reclaimable)
        );
        if groups.is_empty() {
            println!("No byte-for-byte duplicate groups found.");
        }
        for (size, files) in groups {
            println!("\n{} each", human_size(size));
            for file in files {
                println!("  {file}");
            }
        }
    }
    Ok(())
}

fn run_scan(args: &[String]) -> Result<(), String> {
    let mut json = false;
    let mut include_hidden = false;
    let mut path = None;
    let mut kind_filter = None;
    let mut min_size = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--json" => json = true,
            "--include-hidden" => include_hidden = true,
            "--type" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--type requires a value".to_owned())?;
                let valid = [
                    "video", "audio", "image", "model", "texture", "project", "other",
                ];
                if !valid.contains(&value.as_str()) {
                    return Err(format!(
                        "unknown asset type '{value}'; choose from {}",
                        valid.join(", ")
                    ));
                }
                kind_filter = Some(value.clone());
            }
            "--min-size" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--min-size requires a value".to_owned())?;
                min_size = Some(parse_size(value)?);
            }
            "--help" | "-h" => {
                println!(
                    "Usage: mediaforge scan [PATH] [--json] [--include-hidden] [--type TYPE] [--min-size SIZE]\n\nSummarize media and game assets in a folder. Defaults to the current directory.\n\nTypes: video, audio, image, model, texture, project, other\nSizes: bytes, KiB/MiB/GiB, or K/M/G (for example 500M)"
                );
                return Ok(());
            }
            value if value.starts_with('-') => return Err(format!("unknown option: {value}")),
            value => {
                if path.is_some() {
                    return Err("only one scan path may be provided".into());
                }
                path = Some(PathBuf::from(value));
            }
        }
    }
    let path = path.unwrap_or_else(|| PathBuf::from("."));
    let root =
        fs::canonicalize(&path).map_err(|e| format!("cannot access {}: {e}", path.display()))?;
    if !root.is_dir() {
        return Err(format!("expected a directory: {}", root.display()));
    }
    let ignored = if include_hidden {
        HashSet::new()
    } else {
        ignored_by_git(&root)
    };
    let mut records = Vec::new();
    walk(&root, &root, include_hidden, &mut records);
    if !ignored.is_empty() {
        records.retain(|record| !is_ignored(&record.relative, &ignored));
    }
    if let Some(kind) = kind_filter {
        records.retain(|record| record.kind.as_str() == kind);
    }
    if let Some(size) = min_size {
        records.retain(|record| record.size >= size);
    }
    if json {
        print_json(&root, &records);
    } else {
        print_report(&root, &records);
    }
    Ok(())
}

fn parse_size(value: &str) -> Result<u64, String> {
    let normalized = value.trim().to_ascii_lowercase();
    let split = normalized
        .find(|ch: char| !ch.is_ascii_digit() && ch != '.')
        .unwrap_or(normalized.len());
    let (number, unit) = normalized.split_at(split);
    let number: f64 = number
        .parse()
        .map_err(|_| format!("invalid size: {value}"))?;
    if !number.is_finite() || number < 0.0 {
        return Err(format!("invalid size: {value}"));
    }
    let multiplier = match unit.trim() {
        "" | "b" => 1.0,
        "k" | "kb" | "kib" => 1024.0,
        "m" | "mb" | "mib" => 1024.0_f64.powi(2),
        "g" | "gb" | "gib" => 1024.0_f64.powi(3),
        "t" | "tb" | "tib" => 1024.0_f64.powi(4),
        other => return Err(format!("unknown size unit '{other}' in '{value}'")),
    };
    let bytes = number * multiplier;
    if bytes > u64::MAX as f64 {
        return Err(format!("size is too large: {value}"));
    }
    Ok(bytes.ceil() as u64)
}

fn print_help() {
    println!(
        "MediaForge {VERSION}\nFast, local-first CLI for game and video production assets.\n\nUsage:\n  mediaforge scan [PATH] [--json] [--include-hidden] [--type TYPE] [--min-size SIZE]\n  mediaforge dupes [PATH] [--json] [--min-size SIZE]\n  mediaforge probe <FILE> [--compact]\n  mediaforge --version\n  mediaforge --help"
    );
}

fn run_probe(args: &[String]) -> Result<(), String> {
    let mut path = None;
    let mut pretty = true;
    for arg in args {
        match arg.as_str() {
            "--compact" => pretty = false,
            "--help" | "-h" => {
                println!(
                    "Usage: mediaforge probe <FILE> [--compact]\n\nRead duration, stream, codec, resolution and audio metadata with ffprobe."
                );
                return Ok(());
            }
            value if value.starts_with('-') => return Err(format!("unknown option: {value}")),
            value => {
                if path.is_some() {
                    return Err("probe accepts one file path".into());
                }
                path = Some(PathBuf::from(value));
            }
        }
    }
    let path = path.ok_or_else(|| "probe requires a file path".to_owned())?;
    if !path.is_file() {
        return Err(format!("file does not exist: {}", path.display()));
    }

    let mut command = Command::new("ffprobe");
    command.args(["-v", "error", "-show_format", "-show_streams"]);
    if pretty {
        command.args(["-of", "json"]);
    } else {
        command.args(["-of", "json=compact=1"]);
    }
    let output = command.arg(&path).output().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            "ffprobe was not found on PATH. Install FFmpeg and ensure ffprobe is available."
                .to_owned()
        } else {
            format!("could not start ffprobe: {error}")
        }
    })?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(if detail.is_empty() {
            format!("ffprobe failed with {}", output.status)
        } else {
            format!("ffprobe failed: {detail}")
        });
    }
    let json = String::from_utf8(output.stdout)
        .map_err(|error| format!("ffprobe returned invalid UTF-8: {error}"))?;
    print!("{json}");
    Ok(())
}

fn run() -> Result<(), String> {
    let args: Vec<String> = env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--version") | Some("-V") => {
            println!("mediaforge {VERSION}");
            Ok(())
        }
        Some("--help") | Some("-h") | None => {
            print_help();
            Ok(())
        }
        Some("scan") => run_scan(&args[1..]),
        Some("dupes") => run_dupes(&args[1..]),
        Some("probe") => run_probe(&args[1..]),
        Some(command) => Err(format!(
            "unknown command: {command}\nRun 'mediaforge --help' for usage."
        )),
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("mediaforge: {message}");
            ExitCode::from(2)
        }
    }
}
