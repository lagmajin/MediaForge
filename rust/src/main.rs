use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Cursor, Read, Write};
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
const PACK_GENERATED_DIRS: &[&str] = &[
    "Library",
    "Temp",
    "Obj",
    "Intermediate",
    "DerivedDataCache",
    "Binaries",
    ".godot",
];
const PROJECT_ARTIFACT_DIRS: &[&str] = &[
    "Library",
    "Temp",
    "Obj",
    "Intermediate",
    "DerivedDataCache",
    "Binaries",
    "Saved",
    ".godot",
];

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
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
    path: PathBuf,
    size: u64,
    modified: Option<std::time::SystemTime>,
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
    walk_with_skip_dirs(root, current, include_hidden, SKIP_DIRS, records);
}

fn walk_with_skip_dirs(
    root: &Path,
    current: &Path,
    include_hidden: bool,
    skip_dirs: &[&str],
    records: &mut Vec<FileRecord>,
) {
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
            if skip_dirs.contains(&name.as_str()) || (!include_hidden && is_hidden(&name)) {
                continue;
            }
            walk_with_skip_dirs(root, &path, include_hidden, skip_dirs, records);
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
            let kind = classify(&path);
            records.push(FileRecord {
                relative,
                path,
                size: metadata.len(),
                modified: metadata.modified().ok(),
                kind,
                extension,
            });
        }
    }
}

fn walk_strict(
    root: &Path,
    current: &Path,
    include_hidden: bool,
    include_generated: bool,
    skip_dirs: &[&str],
    records: &mut Vec<FileRecord>,
) -> Result<(), String> {
    let entries = fs::read_dir(current)
        .map_err(|error| format!("cannot read directory {}: {error}", current.display()))?;
    let mut entries = entries
        .map(|entry| {
            entry.map_err(|error| format!("cannot read entry in {}: {error}", current.display()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name().to_string_lossy().to_lowercase());
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let file_type = entry
            .file_type()
            .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            let generated_hidden =
                include_generated && PACK_GENERATED_DIRS.contains(&name.as_str());
            if skip_dirs.contains(&name.as_str())
                || (!include_hidden && is_hidden(&name) && !generated_hidden)
            {
                continue;
            }
            walk_strict(
                root,
                &path,
                include_hidden,
                include_generated,
                skip_dirs,
                records,
            )?;
        } else if file_type.is_file() && (include_hidden || !is_hidden(&name)) {
            let metadata = entry
                .metadata()
                .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
            let relative = path
                .strip_prefix(root)
                .map_err(|error| format!("cannot relativize {}: {error}", path.display()))?
                .to_string_lossy()
                .replace('\\', "/");
            let extension = path
                .extension()
                .and_then(|value| value.to_str())
                .map(|value| format!(".{}", value.to_ascii_lowercase()))
                .unwrap_or_else(|| "[no extension]".to_owned());
            records.push(FileRecord {
                relative,
                path: path.clone(),
                size: metadata.len(),
                modified: metadata.modified().ok(),
                kind: classify(&path),
                extension,
            });
        }
    }
    Ok(())
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
        "MediaForge {VERSION}\nFast, local-first CLI for game and video production assets.\n\nUsage:\n  mediaforge scan [PATH] [--json] [--include-hidden] [--type TYPE] [--min-size SIZE]\n  mediaforge dupes [PATH] [--json] [--min-size SIZE]\n  mediaforge probe <FILE> [--compact]\n  mediaforge project [PATH] [--json]\n  mediaforge organize [PATH] [--by type|extension] [--apply]\n  mediaforge undo [PATH]\n  mediaforge pack <PATH> --output <FILE.tar> [--type TYPES] [--include-hidden] [--include-generated] [--dry-run]\n  mediaforge --version\n  mediaforge --help"
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

fn directory_size(root: &Path) -> io::Result<(u64, u64)> {
    let mut pending = vec![root.to_path_buf()];
    let mut bytes = 0_u64;
    let mut files = 0_u64;
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                let name = entry.file_name();
                if !SKIP_DIRS.contains(&name.to_string_lossy().as_ref()) {
                    pending.push(entry.path());
                }
            } else if file_type.is_file() {
                bytes = bytes.saturating_add(entry.metadata()?.len());
                files += 1;
            }
        }
    }
    Ok((bytes, files))
}

fn project_file_kind(path: &str) -> Option<&'static str> {
    match Path::new(path)
        .extension()?
        .to_str()?
        .to_ascii_lowercase()
        .as_str()
    {
        "blend" => Some("Blender"),
        "drp" => Some("DaVinci Resolve"),
        "prproj" => Some("Adobe Premiere Pro"),
        "aep" => Some("Adobe After Effects"),
        "fcpxml" => Some("Final Cut Pro XML"),
        _ => None,
    }
}

fn run_project(args: &[String]) -> Result<(), String> {
    let mut json = false;
    let mut path = None;
    for arg in args {
        match arg.as_str() {
            "--json" => json = true,
            "--help" | "-h" => {
                println!(
                    "Usage: mediaforge project [PATH] [--json]\n\nInspect Unity, Unreal, Godot, Blender and video-editor project markers and report generated cache sizes. This command is read-only."
                );
                return Ok(());
            }
            value if value.starts_with('-') => return Err(format!("unknown option: {value}")),
            value => {
                if path.is_some() {
                    return Err("only one project path may be provided".into());
                }
                path = Some(PathBuf::from(value));
            }
        }
    }
    let path = path.unwrap_or_else(|| PathBuf::from("."));
    let root = fs::canonicalize(&path)
        .map_err(|error| format!("cannot access {}: {error}", path.display()))?;
    if !root.is_dir() {
        return Err(format!("expected a project directory: {}", root.display()));
    }

    let mut engines = Vec::<(&str, Vec<&str>, Vec<&str>)>::new();
    if root.join("Assets").is_dir() || root.join("ProjectSettings/ProjectVersion.txt").is_file() {
        engines.push((
            "Unity",
            vec!["Assets", "ProjectSettings"],
            vec!["Library", "Temp", "Obj"],
        ));
    }
    let unity_version = fs::read_to_string(root.join("ProjectSettings/ProjectVersion.txt"))
        .ok()
        .and_then(|contents| {
            contents.lines().find_map(|line| {
                line.trim()
                    .strip_prefix("m_EditorVersion:")
                    .map(str::trim)
                    .filter(|version| !version.is_empty())
                    .map(str::to_owned)
            })
        });
    let unreal_files: Vec<_> = fs::read_dir(&root)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("uproject"))
        })
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    if !unreal_files.is_empty() {
        engines.push((
            "Unreal Engine",
            vec!["Content"],
            vec!["Intermediate", "Saved", "DerivedDataCache", "Binaries"],
        ));
    }
    if root.join("project.godot").is_file() {
        engines.push(("Godot", vec!["project.godot"], vec![".godot"]));
    }

    let mut project_files = Vec::<(String, &'static str)>::new();
    let ignored = ignored_by_git(&root);
    let mut pending = vec![root.clone()];
    while let Some(directory) = pending.pop() {
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(_) => continue,
            };
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                let name = entry.file_name();
                let lossy_name = name.to_string_lossy();
                let relative = path
                    .strip_prefix(&root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                if !SKIP_DIRS.contains(&lossy_name.as_ref())
                    && !PROJECT_ARTIFACT_DIRS.contains(&lossy_name.as_ref())
                    && !is_hidden(&lossy_name)
                    && !is_ignored(&relative, &ignored)
                {
                    pending.push(path);
                }
            } else if file_type.is_file() {
                let relative = path
                    .strip_prefix(&root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                if is_ignored(&relative, &ignored) {
                    continue;
                }
                if let Some(kind) = project_file_kind(&relative) {
                    project_files.push((relative, kind));
                }
            }
        }
    }
    project_files.sort_by(|left, right| left.0.to_lowercase().cmp(&right.0.to_lowercase()));

    let mut missing = Vec::<(&str, &str)>::new();
    let mut caches = Vec::<(String, u64, u64)>::new();
    for (engine, required, cache_names) in &engines {
        for required_path in required {
            if !root.join(required_path).exists() {
                missing.push((engine, required_path));
            }
        }
        for cache_name in cache_names {
            let cache_path = root.join(cache_name);
            if cache_path.is_dir() {
                let (bytes, files) = directory_size(&cache_path).map_err(|error| {
                    format!("could not inspect {}: {error}", cache_path.display())
                })?;
                caches.push(((*cache_name).to_owned(), bytes, files));
            }
        }
    }

    if json {
        println!("{{\n  \"root\": {},", json_string(&root.to_string_lossy()));
        println!(
            "  \"engines\": [{}],",
            engines
                .iter()
                .map(|(name, _, _)| json_string(name))
                .collect::<Vec<_>>()
                .join(", ")
        );
        println!(
            "  \"unity_version\": {},",
            unity_version
                .as_deref()
                .map(json_string)
                .unwrap_or_else(|| "null".to_owned())
        );
        println!(
            "  \"unreal_project_files\": [{}],",
            unreal_files
                .iter()
                .map(|file| json_string(file))
                .collect::<Vec<_>>()
                .join(", ")
        );
        println!("  \"missing_required_paths\": [");
        for (index, (engine, missing_path)) in missing.iter().enumerate() {
            let comma = if index + 1 == missing.len() { "" } else { "," };
            println!(
                "    {{\"engine\": {}, \"path\": {}}}{}",
                json_string(engine),
                json_string(missing_path),
                comma
            );
        }
        println!("  ],\n  \"known_build_or_cache_dirs\": [");
        for (index, (name, bytes, files)) in caches.iter().enumerate() {
            let comma = if index + 1 == caches.len() { "" } else { "," };
            println!(
                "    {{\"path\": {}, \"files\": {}, \"bytes\": {}, \"size\": {}}}{}",
                json_string(name),
                files,
                bytes,
                json_string(&human_size(*bytes)),
                comma
            );
        }
        println!("  ],\n  \"editor_project_files\": [");
        for (index, (relative, kind)) in project_files.iter().enumerate() {
            let comma = if index + 1 == project_files.len() {
                ""
            } else {
                ","
            };
            println!(
                "    {{\"path\": {}, \"kind\": {}}}{}",
                json_string(relative),
                json_string(kind),
                comma
            );
        }
        println!("  ]\n}}");
    } else {
        println!("MediaForge project inspection: {}", root.display());
        if engines.is_empty() {
            println!("Game engine: no Unity, Unreal or Godot project marker found.");
        }
        for (engine, _, _) in &engines {
            println!("Game engine: {engine}");
        }
        if let Some(version) = &unity_version {
            println!("Unity editor version: {version}");
        }
        for project_file in unreal_files {
            println!("Unreal project: {project_file}");
        }
        if !missing.is_empty() {
            println!("\nMissing expected paths");
            for (engine, missing_path) in &missing {
                println!("  {engine}: {missing_path}");
            }
        }
        if caches.is_empty() {
            println!("\nKnown build/cache folders: none found.");
        } else {
            println!("\nKnown build/cache folders (inspection only; may include backups or logs)");
            for (name, bytes, files) in &caches {
                println!("  {name:<20} {:>10}  ({files} files)", human_size(*bytes));
            }
        }
        if !project_files.is_empty() {
            println!("\nEditor project files");
            for (relative, kind) in &project_files {
                println!("  {kind:<24} {relative}");
            }
        }
        if engines.is_empty() && project_files.is_empty() {
            println!("No recognized project files found.");
        }
    }
    Ok(())
}

const ORGANIZE_MANIFEST: &str = ".mediaforge-undo";
const ORGANIZE_MAGIC: &[u8; 8] = b"MFORG002";
const ORGANIZE_MAGIC_V1: &[u8; 8] = b"MFORG001";
const ORGANIZE_MANIFEST_TEMP: &str = ".mediaforge-undo.tmp";
const TAR_BLOCK_SIZE: usize = 512;
const TAR_OCTAL_MAX: u64 = 0o77777777777;

#[derive(Clone, Debug)]
struct PlannedMove {
    from: String,
    to: String,
}

fn validate_relative_path(value: &str) -> Result<PathBuf, String> {
    let path = Path::new(value);
    if value.is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(format!("unsafe path in organize manifest: {value:?}"));
    }
    Ok(path.to_path_buf())
}

fn validate_portable_relative_path(value: &str) -> Result<PathBuf, String> {
    if value
        .split('/')
        .any(|part| part.is_empty() || part == "." || part == "..")
        || value
            .chars()
            .any(|character| matches!(character, '\\' | ':' | '\0'))
    {
        return Err(format!(
            "unsafe portable path in organize manifest: {value:?}"
        ));
    }
    validate_relative_path(value)
}

fn portable_relative_path(path: &Path) -> Option<String> {
    path.components()
        .map(|component| match component {
            std::path::Component::Normal(part) => part.to_str().and_then(|part| {
                if part
                    .chars()
                    .any(|character| matches!(character, '/' | '\\' | ':' | '\0'))
                {
                    None
                } else {
                    Some(part)
                }
            }),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()
        .filter(|parts| !parts.is_empty())
        .map(|parts| parts.join("/"))
}

fn reject_symlink_components(root: &Path, relative: &Path) -> Result<(), String> {
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(part) = component else {
            return Err(format!("unsafe relative path: {}", relative.display()));
        };
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(format!(
                    "refusing to use symlink path: {}",
                    current.display()
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("cannot inspect {}: {error}", current.display())),
        }
    }
    Ok(())
}

fn write_organize_manifest(root: &Path, moves: &[PlannedMove]) -> Result<(), String> {
    let manifest = root.join(ORGANIZE_MANIFEST);
    let temporary = root.join(ORGANIZE_MANIFEST_TEMP);
    if fs::symlink_metadata(&manifest).is_ok() || fs::symlink_metadata(&temporary).is_ok() {
        return Err(format!(
            "an organize undo manifest or recovery file already exists under {}; run 'mediaforge undo {}' first",
            manifest.display(),
            root.display()
        ));
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&manifest)
        .map_err(|error| format!("cannot create undo manifest: {error}"))?;
    let write_result = (|| -> io::Result<()> {
        file.write_all(ORGANIZE_MAGIC)?;
        let count = u32::try_from(moves.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "too many planned moves"))?;
        file.write_all(&count.to_le_bytes())?;
        for movement in moves {
            let from = movement.from.as_bytes();
            let to = movement.to.as_bytes();
            let from_len = u32::try_from(from.len()).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "source path is too long")
            })?;
            let to_len = u32::try_from(to.len()).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "destination path is too long")
            })?;
            file.write_all(&from_len.to_le_bytes())?;
            file.write_all(from)?;
            file.write_all(&to_len.to_le_bytes())?;
            file.write_all(to)?;
        }
        file.sync_all()
    })();
    drop(file);
    if let Err(error) = write_result {
        let _ = fs::remove_file(&manifest);
        return Err(format!("could not write undo manifest: {error}"));
    }
    Ok(())
}

fn read_organize_manifest(root: &Path) -> Result<Vec<PlannedMove>, String> {
    let manifest = root.join(ORGANIZE_MANIFEST);
    let temporary = root.join(ORGANIZE_MANIFEST_TEMP);
    let path = match fs::symlink_metadata(&temporary) {
        Ok(_) => &temporary,
        Err(error) if error.kind() == io::ErrorKind::NotFound => &manifest,
        Err(error) => {
            return Err(format!("cannot inspect temporary undo manifest: {error}"));
        }
    };
    let metadata = fs::symlink_metadata(&path)
        .map_err(|error| format!("no organize undo manifest found: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("undo manifest is not a regular file".to_owned());
    }
    let bytes = fs::read(&path).map_err(|error| format!("cannot read undo manifest: {error}"))?;
    let mut cursor = Cursor::new(bytes.as_slice());
    let mut magic = [0; 8];
    cursor
        .read_exact(&mut magic)
        .map_err(|error| format!("invalid undo manifest header: {error}"))?;
    let portable = if &magic == ORGANIZE_MAGIC {
        true
    } else if &magic == ORGANIZE_MAGIC_V1 {
        false
    } else {
        return Err("unrecognized undo manifest format".to_owned());
    };
    let count = read_manifest_u32(&mut cursor)? as usize;
    if count > 1_000_000 {
        return Err("undo manifest contains too many entries".to_owned());
    }
    let mut moves = Vec::with_capacity(count);
    let mut sources = HashSet::new();
    let mut destinations = HashSet::new();
    for _ in 0..count {
        let from = read_manifest_string(&mut cursor)?;
        let to = read_manifest_string(&mut cursor)?;
        if portable {
            validate_portable_relative_path(&from)?;
            validate_portable_relative_path(&to)?;
        } else {
            validate_relative_path(&from)?;
            validate_relative_path(&to)?;
        }
        if !sources.insert(from.clone()) || !destinations.insert(to.clone()) {
            return Err("undo manifest contains repeated paths".to_owned());
        }
        moves.push(PlannedMove { from, to });
    }
    if cursor.position() != bytes.len() as u64 {
        return Err("undo manifest contains trailing data".to_owned());
    }
    Ok(moves)
}

fn remove_organize_manifests(root: &Path) -> Result<(), String> {
    for name in [ORGANIZE_MANIFEST_TEMP, ORGANIZE_MANIFEST] {
        let path = root.join(name);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "could not remove undo recovery file {}: {error}",
                    path.display()
                ));
            }
        }
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos", not(windows)))]
fn move_file_with_hard_link(from: &Path, to: &Path) -> io::Result<()> {
    fs::hard_link(from, to)?;
    if let Err(error) = fs::remove_file(from) {
        match fs::remove_file(to) {
            Ok(()) => Err(error),
            Err(rollback_error) => Err(io::Error::other(format!(
                "could not remove source ({error}) or roll back destination ({rollback_error})"
            ))),
        }
    } else {
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn move_file_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    const AT_FDCWD: i32 = -100;
    const RENAME_NOREPLACE: u32 = 1;

    unsafe extern "C" {
        fn renameat2(
            olddirfd: i32,
            oldpath: *const std::ffi::c_char,
            newdirfd: i32,
            newpath: *const std::ffi::c_char,
            flags: u32,
        ) -> i32;
    }

    let source = CString::new(from.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "source path contains NUL"))?;
    let destination = CString::new(to.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "destination path contains NUL")
    })?;
    let result = unsafe {
        renameat2(
            AT_FDCWD,
            source.as_ptr(),
            AT_FDCWD,
            destination.as_ptr(),
            RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        let error = io::Error::last_os_error();
        if matches!(error.raw_os_error(), Some(22 | 38 | 95)) {
            move_file_with_hard_link(from, to)
        } else {
            Err(error)
        }
    }
}

#[cfg(target_os = "macos")]
fn move_file_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;

    const RENAME_EXCL: u32 = 0x00000004;

    unsafe extern "C" {
        fn renamex_np(
            oldpath: *const std::ffi::c_char,
            newpath: *const std::ffi::c_char,
            flags: u32,
        ) -> i32;
    }

    let source = std::ffi::CString::new(from.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "source path contains NUL"))?;
    let destination = std::ffi::CString::new(to.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "destination path contains NUL")
    })?;
    let result = unsafe { renamex_np(source.as_ptr(), destination.as_ptr(), RENAME_EXCL) };
    if result == 0 {
        Ok(())
    } else {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Unsupported {
            move_file_with_hard_link(from, to)
        } else {
            Err(error)
        }
    }
}

#[cfg(windows)]
fn move_file_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;

    unsafe extern "system" {
        fn MoveFileExW(
            existing_file_name: *const u16,
            new_file_name: *const u16,
            flags: u32,
        ) -> i32;
    }

    let from = from
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let to = to
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let result = unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), MOVEFILE_WRITE_THROUGH) };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn move_file_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    move_file_with_hard_link(from, to)
}

fn read_manifest_u32(cursor: &mut Cursor<&[u8]>) -> Result<u32, String> {
    let mut bytes = [0; 4];
    cursor
        .read_exact(&mut bytes)
        .map_err(|error| format!("truncated undo manifest: {error}"))?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_manifest_string(cursor: &mut Cursor<&[u8]>) -> Result<String, String> {
    let len = read_manifest_u32(cursor)? as usize;
    if len == 0 || len > 1024 * 1024 {
        return Err("invalid path length in undo manifest".to_owned());
    }
    let mut bytes = vec![0; len];
    cursor
        .read_exact(&mut bytes)
        .map_err(|error| format!("truncated undo manifest path: {error}"))?;
    String::from_utf8(bytes).map_err(|error| format!("undo manifest path is not UTF-8: {error}"))
}

fn run_organize(args: &[String]) -> Result<(), String> {
    let mut path = None;
    let mut by = "type";
    let mut apply = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--by" => {
                by = iter
                    .next()
                    .ok_or_else(|| "--by requires 'type' or 'extension'".to_owned())?;
                if by != "type" && by != "extension" {
                    return Err("--by must be 'type' or 'extension'".to_owned());
                }
            }
            "--apply" => apply = true,
            "--help" | "-h" => {
                println!(
                    "Usage: mediaforge organize [PATH] [--by type|extension] [--apply]\n\nPreview an asset organization plan. Files are only moved with --apply; a manifest is written so the move can be undone."
                );
                return Ok(());
            }
            value if value.starts_with('-') => return Err(format!("unknown option: {value}")),
            value => {
                if path.is_some() {
                    return Err("only one organize path may be provided".into());
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
    if apply && fs::symlink_metadata(root.join(ORGANIZE_MANIFEST_TEMP)).is_ok() {
        return Err(format!(
            "a temporary undo manifest already exists; inspect or remove it before organizing: {}",
            root.join(ORGANIZE_MANIFEST_TEMP).display()
        ));
    }
    if apply && fs::symlink_metadata(root.join(ORGANIZE_MANIFEST)).is_ok() {
        return Err(format!(
            "an undo manifest already exists; undo the previous move first: {}",
            root.display()
        ));
    }

    let ignored = ignored_by_git(&root);
    let mut records = Vec::new();
    walk(&root, &root, false, &mut records);
    records.retain(|record| !is_ignored(&record.relative, &ignored));
    let mut moves = Vec::<PlannedMove>::new();
    let mut skipped = 0usize;
    let mut destinations = HashSet::<PathBuf>::new();
    for record in records {
        let relative = match record.path.strip_prefix(&root) {
            Ok(relative) => relative,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        let Some(from_text) = portable_relative_path(relative) else {
            skipped += 1;
            continue;
        };
        let category = if by == "type" {
            record.kind.as_str().to_owned()
        } else if record.extension == "[no extension]" {
            "no-extension".to_owned()
        } else {
            record.extension.trim_start_matches('.').to_owned()
        };
        let mut components = relative.components();
        if components.next().and_then(|part| part.as_os_str().to_str()) == Some(category.as_str()) {
            continue;
        }
        let from = validate_portable_relative_path(&from_text)?;
        let to = PathBuf::from(&category).join(relative);
        let Some(to_text) = portable_relative_path(&to) else {
            skipped += 1;
            continue;
        };
        let to = validate_portable_relative_path(&to_text)?;
        if !destinations.insert(to.clone()) || fs::symlink_metadata(root.join(&to)).is_ok() {
            skipped += 1;
            continue;
        }
        if reject_symlink_components(&root, &from).is_err()
            || reject_symlink_components(&root, &to).is_err()
        {
            skipped += 1;
            continue;
        }
        moves.push(PlannedMove {
            from: from_text,
            to: to_text,
        });
    }

    println!(
        "Organize preview by {by}: {} file(s) to move; {skipped} skipped",
        moves.len()
    );
    for movement in moves.iter().take(30) {
        println!("  {} -> {}", movement.from, movement.to);
    }
    if moves.len() > 30 {
        println!("  ... and {} more", moves.len() - 30);
    }
    if !apply {
        if !moves.is_empty() {
            println!(
                "\nPreview only. Re-run with --apply to move files; then use 'mediaforge undo {}' to restore them.",
                root.display()
            );
        }
        return Ok(());
    }
    if moves.is_empty() {
        println!("No files need moving.");
        return Ok(());
    }

    for movement in &moves {
        let from = validate_relative_path(&movement.from)?;
        let to = validate_relative_path(&movement.to)?;
        reject_symlink_components(&root, &from)?;
        reject_symlink_components(&root, &to)?;
        if !root.join(&from).is_file() || fs::symlink_metadata(root.join(&to)).is_ok() {
            return Err(format!(
                "file plan changed since preview; refusing to apply: {}",
                movement.from
            ));
        }
    }
    write_organize_manifest(&root, &moves)?;
    let mut moved = 0usize;
    for movement in &moves {
        let from = root.join(validate_relative_path(&movement.from)?);
        let to = root.join(validate_relative_path(&movement.to)?);
        let parent = to
            .parent()
            .ok_or_else(|| "destination has no parent directory".to_owned())?;
        if let Err(error) =
            fs::create_dir_all(parent).and_then(|()| move_file_no_replace(&from, &to))
        {
            return Err(format!(
                "moved {moved} file(s) before failure: {error}; recovery manifest remains at {}",
                root.join(ORGANIZE_MANIFEST).display()
            ));
        }
        moved += 1;
    }
    println!(
        "Moved {moved} file(s). Undo manifest: {}",
        root.join(ORGANIZE_MANIFEST).display()
    );
    Ok(())
}

fn run_undo(args: &[String]) -> Result<(), String> {
    let mut path = None;
    for arg in args {
        match arg.as_str() {
            "--help" | "-h" => {
                println!(
                    "Usage: mediaforge undo [PATH]\n\nRestore files moved by the most recent 'mediaforge organize --apply' in PATH."
                );
                return Ok(());
            }
            value if value.starts_with('-') => return Err(format!("unknown option: {value}")),
            value => {
                if path.is_some() {
                    return Err("only one undo path may be provided".into());
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
    let moves = read_organize_manifest(&root)?;
    let mut restored = 0usize;
    let mut conflicts = 0usize;
    for movement in moves.iter().rev() {
        let from = root.join(validate_relative_path(&movement.from)?);
        let to = root.join(validate_relative_path(&movement.to)?);
        reject_symlink_components(&root, &PathBuf::from(&movement.from))?;
        reject_symlink_components(&root, &PathBuf::from(&movement.to))?;
        let from_exists = fs::symlink_metadata(&from).is_ok();
        let to_exists = fs::symlink_metadata(&to).is_ok();
        if !from_exists && to_exists {
            let parent = from
                .parent()
                .ok_or_else(|| "source has no parent directory".to_owned())?;
            if let Err(error) =
                fs::create_dir_all(parent).and_then(|()| move_file_no_replace(&to, &from))
            {
                eprintln!("mediaforge: could not restore {}: {error}", movement.from);
                conflicts += 1;
            } else {
                restored += 1;
            }
        } else if from_exists && to_exists {
            eprintln!(
                "mediaforge: conflict; both source and organized file exist: {}",
                movement.from
            );
            conflicts += 1;
        } else if !from_exists && !to_exists {
            eprintln!(
                "mediaforge: missing both source and organized file: {}",
                movement.from
            );
            conflicts += 1;
        }
    }
    if conflicts == 0 {
        remove_organize_manifests(&root)?;
        println!("Restored {restored} file(s); undo manifest removed.");
    } else {
        println!(
            "Restored {restored} file(s); {conflicts} conflict(s) remain. Manifest retained for recovery."
        );
    }
    Ok(())
}

fn ustar_name(path: &[u8]) -> Option<(&[u8], &[u8])> {
    if path.len() <= 100 {
        return Some((path, &[]));
    }
    for (index, byte) in path.iter().enumerate().rev() {
        if *byte == b'/' && index <= 155 && path.len() - index - 1 <= 100 {
            return Some((&path[index + 1..], &path[..index]));
        }
    }
    None
}

fn write_tar_octal(field: &mut [u8], value: u64) -> Result<(), String> {
    let digits = format!("{value:o}");
    if digits.len() > field.len().saturating_sub(1) {
        return Err("tar numeric field overflow".to_owned());
    }
    field.fill(b'0');
    let start = field.len() - digits.len() - 1;
    field[start..start + digits.len()].copy_from_slice(digits.as_bytes());
    field[field.len() - 1] = 0;
    Ok(())
}

fn tar_header(
    name: &[u8],
    prefix: &[u8],
    mode: u32,
    size: u64,
    modified: u64,
    kind: u8,
) -> Result<[u8; TAR_BLOCK_SIZE], String> {
    if name.len() > 100 || prefix.len() > 155 {
        return Err("tar path header field is too long".to_owned());
    }
    let mut header = [0_u8; TAR_BLOCK_SIZE];
    header[..name.len()].copy_from_slice(name);
    write_tar_octal(&mut header[100..108], mode as u64)?;
    write_tar_octal(&mut header[108..116], 0)?;
    write_tar_octal(&mut header[116..124], 0)?;
    write_tar_octal(&mut header[124..136], size)?;
    write_tar_octal(&mut header[136..148], modified)?;
    header[148..156].fill(b' ');
    header[156] = kind;
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    header[265..265 + b"mediaforge".len()].copy_from_slice(b"mediaforge");
    if !prefix.is_empty() {
        header[345..345 + prefix.len()].copy_from_slice(prefix);
    }
    let checksum: u64 = header.iter().map(|byte| *byte as u64).sum();
    if checksum > 0o777777 {
        return Err("tar header checksum overflow".to_owned());
    }
    let checksum_text = format!("{checksum:06o}\0 ");
    header[148..156].copy_from_slice(checksum_text.as_bytes());
    Ok(header)
}

fn pax_record(key: &str, value: &str) -> String {
    let body = format!(" {key}={value}\n");
    let mut length = body.len() + 1;
    loop {
        let updated = body.len() + length.to_string().len();
        if updated == length {
            return format!("{length}{body}");
        }
        length = updated;
    }
}

fn write_tar_payload(writer: &mut File, contents: &[u8]) -> io::Result<()> {
    writer.write_all(contents)?;
    let padding = (TAR_BLOCK_SIZE - contents.len() % TAR_BLOCK_SIZE) % TAR_BLOCK_SIZE;
    if padding > 0 {
        writer.write_all(&[0_u8; TAR_BLOCK_SIZE][..padding])?;
    }
    Ok(())
}

fn pack_file(
    writer: &mut File,
    archive_name: &str,
    record: &FileRecord,
    index: usize,
) -> Result<(), String> {
    let path_bytes = archive_name.as_bytes();
    let split = ustar_name(path_bytes);
    let pax_path = split.is_none();
    let source_metadata = fs::symlink_metadata(&record.path)
        .map_err(|error| format!("cannot inspect {}: {error}", record.path.display()))?;
    if !source_metadata.is_file() || source_metadata.file_type().is_symlink() {
        return Err(format!(
            "source changed type since scan: {}",
            record.path.display()
        ));
    }
    let source_modified = source_metadata.modified().ok();
    if source_metadata.len() != record.size || source_modified != record.modified {
        return Err(format!(
            "source changed since scan: {}",
            record.path.display()
        ));
    }
    let mode = archive_mode(&source_metadata);
    let modified = source_modified
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |duration| duration.as_secs());
    let pax_size = record.size > TAR_OCTAL_MAX;
    let pax_mtime = modified > TAR_OCTAL_MAX;

    if pax_path || pax_size || pax_mtime {
        let mut pax = String::new();
        if pax_path {
            pax.push_str(&pax_record("path", archive_name));
        }
        if pax_size {
            pax.push_str(&pax_record("size", &record.size.to_string()));
        }
        if pax_mtime {
            pax.push_str(&pax_record("mtime", &modified.to_string()));
        }
        let pax_name = format!("PaxHeaders/MediaForge/{index}");
        let header = tar_header(pax_name.as_bytes(), &[], 0o644, pax.len() as u64, 0, b'x')?;
        writer
            .write_all(&header)
            .and_then(|()| write_tar_payload(writer, pax.as_bytes()))
            .map_err(|error| format!("could not write PAX header: {error}"))?;
    }

    let (header_name, prefix) = if pax_path {
        (format!("PaxFiles/{index}"), "".to_owned())
    } else {
        let (name, prefix) = split.expect("checked USTAR path split");
        (
            String::from_utf8_lossy(name).into_owned(),
            String::from_utf8_lossy(prefix).into_owned(),
        )
    };
    let header_size = if pax_size { 0 } else { record.size };
    let header_mtime = modified.min(TAR_OCTAL_MAX);
    let header = tar_header(
        header_name.as_bytes(),
        prefix.as_bytes(),
        mode,
        header_size,
        header_mtime,
        b'0',
    )?;
    writer
        .write_all(&header)
        .map_err(|error| format!("could not write tar header: {error}"))?;

    let mut input = File::open(&record.path)
        .map_err(|error| format!("cannot open {}: {error}", record.path.display()))?;
    let mut limited = (&mut input).take(record.size);
    let copied = io::copy(&mut limited, writer)
        .map_err(|error| format!("could not archive {}: {error}", record.path.display()))?;
    if copied != record.size {
        return Err(format!(
            "source changed while packing: {}",
            record.path.display()
        ));
    }
    let mut extra = [0_u8; 1];
    if input
        .read(&mut extra)
        .map_err(|error| format!("could not verify {}: {error}", record.path.display()))?
        != 0
    {
        return Err(format!(
            "source grew while packing: {}",
            record.path.display()
        ));
    }
    let final_metadata = input
        .metadata()
        .map_err(|error| format!("could not verify {}: {error}", record.path.display()))?;
    if final_metadata.len() != record.size || final_metadata.modified().ok() != record.modified {
        return Err(format!(
            "source changed while packing: {}",
            record.path.display()
        ));
    }
    let padding =
        (TAR_BLOCK_SIZE as u64 - record.size % TAR_BLOCK_SIZE as u64) % TAR_BLOCK_SIZE as u64;
    if padding > 0 {
        writer
            .write_all(&[0_u8; TAR_BLOCK_SIZE][..padding as usize])
            .map_err(|error| format!("could not pad tar member: {error}"))?;
    }
    Ok(())
}

#[cfg(unix)]
fn archive_mode(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o777
}

#[cfg(not(unix))]
fn archive_mode(_: &fs::Metadata) -> u32 {
    0o644
}

fn parse_pack_types(value: &str) -> Result<HashSet<Kind>, String> {
    let mut kinds = HashSet::new();
    for name in value.split(',').map(str::trim) {
        if name.is_empty() {
            return Err("--type contains an empty value".to_owned());
        }
        let kind = match name.to_ascii_lowercase().as_str() {
            "video" => Kind::Video,
            "audio" => Kind::Audio,
            "image" => Kind::Image,
            "model" => Kind::Model,
            "texture" => Kind::Texture,
            "project" => Kind::Project,
            "other" => Kind::Other,
            _ => {
                return Err(format!(
                    "unknown pack type '{name}'; choose video, audio, image, model, texture, project, or other"
                ));
            }
        };
        kinds.insert(kind);
    }
    Ok(kinds)
}

fn run_pack(args: &[String]) -> Result<(), String> {
    let mut path = None;
    let mut output = None;
    let mut include_hidden = false;
    let mut include_generated = false;
    let mut dry_run = false;
    let mut selected_types: Option<HashSet<Kind>> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--output" | "-o" => {
                output = Some(PathBuf::from(
                    iter.next()
                        .ok_or_else(|| "--output requires a file path".to_owned())?,
                ));
            }
            "--include-hidden" => include_hidden = true,
            "--include-generated" => include_generated = true,
            "--dry-run" => dry_run = true,
            "--type" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--type requires a comma-separated type list".to_owned())?;
                selected_types = Some(parse_pack_types(value)?);
            }
            "--help" | "-h" => {
                println!(
                    "Usage: mediaforge pack <PATH> --output <FILE.tar> [--type TYPES] [--include-hidden] [--include-generated] [--dry-run]\n\nCreate an uncompressed, stream-written tar archive. --type accepts comma-separated scan types: video, audio, image, model, texture, project, other. Git-ignored and hidden files are skipped by default. Common Unity, Unreal, and Godot generated folders are excluded unless --include-generated is used. PAX extensions support long paths and files larger than 8 GiB."
                );
                return Ok(());
            }
            value if value.starts_with('-') => return Err(format!("unknown option: {value}")),
            value => {
                if path.is_some() {
                    return Err("pack accepts one input directory".into());
                }
                path = Some(PathBuf::from(value));
            }
        }
    }
    let path = path.ok_or_else(|| "pack requires an input directory".to_owned())?;
    let output = output.ok_or_else(|| "pack requires --output <FILE.tar>".to_owned())?;
    let root = fs::canonicalize(&path)
        .map_err(|error| format!("cannot access {}: {error}", path.display()))?;
    if !root.is_dir() {
        return Err(format!("expected a project directory: {}", root.display()));
    }
    let requested_name = output
        .file_name()
        .ok_or_else(|| "output must name a file".to_owned())?;
    let requested_parent = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let output_parent = fs::canonicalize(requested_parent).map_err(|error| {
        format!(
            "cannot access output directory {}: {error}",
            requested_parent.display()
        )
    })?;
    let output_path = output_parent.join(requested_name);
    if output_path.starts_with(&root) {
        return Err("archive output must be outside the input directory".to_owned());
    }
    let output_exists = fs::symlink_metadata(&output_path).is_ok();
    if output_exists && !dry_run {
        return Err(format!(
            "output already exists; refusing to overwrite: {}",
            output_path.display()
        ));
    }

    let ignored = ignored_by_git(&root);
    let mut records = Vec::new();
    let mut skip_dirs = SKIP_DIRS.to_vec();
    if !include_generated {
        skip_dirs.extend_from_slice(PACK_GENERATED_DIRS);
    }
    walk_strict(
        &root,
        &root,
        include_hidden,
        include_generated,
        &skip_dirs,
        &mut records,
    )?;
    records.retain(|record| !is_ignored(&record.relative, &ignored));
    if let Some(types) = &selected_types {
        records.retain(|record| types.contains(&record.kind));
    }
    let mut entries = Vec::<(&FileRecord, String)>::with_capacity(records.len());
    let mut total_bytes = 0_u64;
    for record in &records {
        let relative = record
            .path
            .strip_prefix(&root)
            .map_err(|error| format!("cannot relativize {}: {error}", record.path.display()))?;
        let mut components = Vec::new();
        for component in relative.components() {
            let std::path::Component::Normal(part) = component else {
                return Err(format!("unsafe archive path: {}", relative.display()));
            };
            let part = part.to_str().ok_or_else(|| {
                format!("non-UTF-8 path cannot be packed: {}", relative.display())
            })?;
            if part
                .chars()
                .any(|character| matches!(character, '\\' | ':' | '\0'))
            {
                return Err(format!(
                    "path contains a character unsafe for cross-platform extraction: {}",
                    relative.display()
                ));
            }
            components.push(part);
        }
        if components.is_empty() {
            return Err("cannot archive the input directory itself".to_owned());
        }
        let archive_name = components.join("/");
        if archive_name.len() > 1024 * 1024 {
            return Err(format!("archive path is too long: {archive_name}"));
        }
        entries.push((record, archive_name));
        total_bytes = total_bytes
            .checked_add(record.size)
            .ok_or_else(|| "total input size exceeds the supported range".to_owned())?;
    }
    println!(
        "Pack plan: {} file(s), {} uncompressed",
        entries.len(),
        human_size(total_bytes)
    );
    for (record, name) in entries.iter().take(30) {
        println!("  {}  {name}", human_size(record.size));
    }
    if entries.len() > 30 {
        println!("  ... and {} more", entries.len() - 30);
    }
    if dry_run {
        if output_exists {
            println!(
                "Dry run; no archive was written. Output already exists and would not be overwritten."
            );
        } else {
            println!("Dry run; would create {}", output_path.display());
        }
        return Ok(());
    }

    let mut archive = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&output_path)
        .map_err(|error| format!("cannot create archive {}: {error}", output_path.display()))?;
    let write_result = (|| -> Result<(), String> {
        for (index, (record, name)) in entries.iter().enumerate() {
            pack_file(&mut archive, name, record, index + 1)?;
        }
        archive
            .write_all(&[0_u8; TAR_BLOCK_SIZE * 2])
            .map_err(|error| format!("could not finish tar archive: {error}"))?;
        archive
            .sync_all()
            .map_err(|error| format!("could not flush archive: {error}"))
    })();
    drop(archive);
    if let Err(error) = write_result {
        return match fs::remove_file(&output_path) {
            Ok(()) => Err(error),
            Err(cleanup_error) if cleanup_error.kind() == io::ErrorKind::NotFound => Err(error),
            Err(cleanup_error) => Err(format!(
                "{error}; incomplete archive could not be removed at {}: {cleanup_error}",
                output_path.display()
            )),
        };
    }
    let archive_size = fs::metadata(&output_path)
        .map_err(|error| format!("archive was created but could not be inspected: {error}"))?
        .len();
    println!(
        "Created {} ({})",
        output_path.display(),
        human_size(archive_size)
    );
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
        Some("project") => run_project(&args[1..]),
        Some("organize") => run_organize(&args[1..]),
        Some("undo") => run_undo(&args[1..]),
        Some("pack") => run_pack(&args[1..]),
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
