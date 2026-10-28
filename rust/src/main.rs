use std::borrow::Cow;
use std::collections::{BTreeMap, BinaryHeap, HashMap, HashSet, VecDeque};
use std::env;
use std::fmt::Write as FmtWrite;
use std::fs::{self, File, OpenOptions};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{self, BufRead, BufReader, BufWriter, Cursor, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::{Arc, LazyLock};

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
    ".import",
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
    ".import",
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

#[derive(Clone, Copy)]
enum ScanCsvSort {
    Path,
    Size,
    Modified,
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

fn parse_asset_kind(value: &str) -> Option<Kind> {
    match value.to_ascii_lowercase().as_str() {
        "video" => Some(Kind::Video),
        "audio" => Some(Kind::Audio),
        "image" => Some(Kind::Image),
        "model" => Some(Kind::Model),
        "texture" => Some(Kind::Texture),
        "project" => Some(Kind::Project),
        "other" => Some(Kind::Other),
        _ => None,
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

struct SnapshotFileRecord {
    relative: String,
    path: PathBuf,
    size: u64,
    modified: Option<std::time::SystemTime>,
}

struct ScanFileRecord {
    relative: String,
    size: u64,
    modified: Option<std::time::SystemTime>,
    kind: Kind,
    extension: String,
}

struct JsonLargestFile {
    relative: String,
    size: u64,
    kind: Kind,
}

impl PartialEq for JsonLargestFile {
    fn eq(&self, other: &Self) -> bool {
        compare_largest_values(self.size, &self.relative, other.size, &other.relative).is_eq()
    }
}

impl Eq for JsonLargestFile {}

impl PartialOrd for JsonLargestFile {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for JsonLargestFile {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        compare_largest_values(self.size, &self.relative, other.size, &other.relative)
    }
}

struct ScanJsonAccumulator {
    file_count: usize,
    total_bytes: u128,
    by_type: BTreeMap<String, (u64, u128)>,
    extensions: BTreeMap<String, (usize, u128)>,
    size_counts: HashMap<u64, u8>,
    candidate_group_count: usize,
    largest_limit: usize,
    largest: BinaryHeap<JsonLargestFile>,
}

impl ScanJsonAccumulator {
    fn new(largest_limit: usize) -> Self {
        Self {
            file_count: 0,
            total_bytes: 0,
            by_type: BTreeMap::new(),
            extensions: BTreeMap::new(),
            size_counts: HashMap::new(),
            candidate_group_count: 0,
            largest_limit,
            largest: BinaryHeap::with_capacity(largest_limit.min(1024)),
        }
    }

    fn add(&mut self, record: &ScanFileRecord) {
        self.file_count = self.file_count.saturating_add(1);
        self.total_bytes += u128::from(record.size);
        let by_type = self
            .by_type
            .entry(record.kind.as_str().to_owned())
            .or_default();
        by_type.0 += 1;
        by_type.1 += u128::from(record.size);
        let extension = self.extensions.entry(record.extension.clone()).or_default();
        extension.0 += 1;
        extension.1 += u128::from(record.size);
        let size_count = self.size_counts.entry(record.size).or_default();
        if *size_count == 1 {
            self.candidate_group_count = self.candidate_group_count.saturating_add(1);
        }
        *size_count = size_count.saturating_add(1);

        if self.largest_limit == 0 {
            return;
        }
        let should_keep = self.largest.len() < self.largest_limit
            || self.largest.peek().is_some_and(|smallest| {
                compare_largest_values(
                    record.size,
                    &record.relative,
                    smallest.size,
                    &smallest.relative,
                )
                .is_lt()
            });
        if should_keep {
            if self.largest.len() == self.largest_limit {
                self.largest.pop();
            }
            self.largest.push(JsonLargestFile {
                relative: record.relative.clone(),
                size: record.size,
                kind: record.kind,
            });
        }
    }

    fn largest_sorted(&mut self) -> Vec<JsonLargestFile> {
        let mut largest = self.largest.drain().collect::<Vec<_>>();
        largest.sort_unstable();
        largest
    }
}

enum ScanRecordConsumer<'a> {
    Csv(&'a mut dyn Write),
    Json(&'a mut ScanJsonAccumulator),
}

struct FilePathRecord {
    relative: String,
    path: PathBuf,
}

struct ScanTraversalFilters<'a> {
    kind: Option<Kind>,
    min_size: Option<u64>,
    max_size: Option<u64>,
    modified_since: Option<u64>,
    modified_until: Option<u64>,
    include_modified: bool,
    extensions: Option<&'a HashSet<String>>,
    path_contains_lower: &'a [String],
    exclude_path_contains_lower: &'a [String],
    sort_entries_per_directory: bool,
}

static ASSET_KIND_BY_EXTENSION: LazyLock<Vec<(&'static str, Kind)>> = LazyLock::new(|| {
    const VIDEO: &[&str] = &[
        "mp4", "mov", "mxf", "mkv", "avi", "webm", "m4v", "mpg", "mpeg", "mpe", "mpv", "m2v", "ts",
        "mts", "m2ts", "vob", "ogv", "3gp", "3g2", "wmv", "flv", "f4v", "asf", "divx", "r3d",
        "braw", "crm",
    ];
    const AUDIO: &[&str] = &[
        "wav", "aiff", "aif", "flac", "mp3", "ogg", "m4a", "aac", "caf", "ac3", "eac3", "alac",
        "opus", "wma", "amr", "mid", "midi", "wem", "bnk", "bank", "fsb",
    ];
    const IMAGE: &[&str] = &[
        "png", "jpg", "jpeg", "tif", "tiff", "exr", "hdr", "tga", "bmp", "webp", "gif", "heic",
        "heif", "jp2", "j2k", "dpx", "cin", "dng", "cr2", "cr3", "nef", "arw", "raf", "orf", "rw2",
        "ari",
    ];
    const MODEL: &[&str] = &[
        "fbx", "obj", "gltf", "glb", "usd", "usda", "usdc", "usdz", "abc", "ma", "mb", "stl",
        "ply", "3ds", "dae",
    ];
    const TEXTURE: &[&str] = &[
        "sbsar", "psd", "psb", "ktx", "ktx2", "dds", "material", "basis", "pvr", "astc",
    ];
    const PROJECT: &[&str] = &[
        "uproject",
        "uasset",
        "umap",
        "unity",
        "unitypackage",
        "asset",
        "prefab",
        "godot",
        "tscn",
        "tres",
        "scn",
        "res",
        "pak",
        "utoc",
        "ucas",
        "pck",
        "wproj",
        "wwu",
        "fspro",
        "drp",
        "dra",
        "prproj",
        "aep",
        "aepx",
        "aaf",
        "edl",
        "mlt",
        "fcpxml",
        "blend",
        "sbs",
        "spp",
    ];
    let mut kinds = Vec::with_capacity(
        VIDEO.len() + AUDIO.len() + IMAGE.len() + MODEL.len() + TEXTURE.len() + PROJECT.len(),
    );
    for (extensions, kind) in [
        (VIDEO, Kind::Video),
        (AUDIO, Kind::Audio),
        (IMAGE, Kind::Image),
        (MODEL, Kind::Model),
        (TEXTURE, Kind::Texture),
        (PROJECT, Kind::Project),
    ] {
        for extension in extensions {
            kinds.push((*extension, kind));
        }
    }
    kinds.sort_unstable_by_key(|(extension, _)| *extension);
    kinds
});

fn compare_extensions_case_insensitive(left: &str, right: &str) -> std::cmp::Ordering {
    left.bytes()
        .map(|byte| byte.to_ascii_lowercase())
        .cmp(right.bytes().map(|byte| byte.to_ascii_lowercase()))
}

fn classify(path: &Path) -> Kind {
    classify_extension(path.extension().and_then(|extension| extension.to_str()))
}

fn classify_extension(extension: Option<&str>) -> Kind {
    let Some(extension) = extension else {
        return Kind::Other;
    };
    ASSET_KIND_BY_EXTENSION
        .binary_search_by(|(candidate, _)| {
            compare_extensions_case_insensitive(candidate, extension)
        })
        .ok()
        .map_or(Kind::Other, |index| ASSET_KIND_BY_EXTENSION[index].1)
}

fn matches_extension_filter(extension: Option<&str>, filters: &HashSet<String>) -> bool {
    const LINEAR_FILTER_LIMIT: usize = 8;
    match extension {
        Some(extension) if filters.len() > LINEAR_FILTER_LIMIT => {
            let extension_bytes = extension.as_bytes();
            let mut normalized = [0_u8; 256];
            if extension_bytes.len() < normalized.len() {
                normalized[0] = b'.';
                for (target, source) in normalized[1..1 + extension_bytes.len()]
                    .iter_mut()
                    .zip(extension_bytes)
                {
                    *target = source.to_ascii_lowercase();
                }
                let candidate = std::str::from_utf8(&normalized[..1 + extension_bytes.len()])
                    .expect("normalized extension remains valid UTF-8");
                filters.contains(candidate)
            } else {
                filters.contains(&format!(".{}", extension.to_ascii_lowercase()))
            }
        }
        Some(extension) => filters.iter().any(|candidate| {
            candidate
                .strip_prefix('.')
                .is_some_and(|candidate| candidate.eq_ignore_ascii_case(extension))
        }),
        None => filters.contains("[no extension]"),
    }
}

fn lowercase_into(value: &str, output: &mut String) {
    output.clear();
    output.extend(value.chars().flat_map(char::to_lowercase));
}

fn human_size(size: u64) -> String {
    human_size_wide(u128::from(size))
}

fn human_size_wide(size: u128) -> String {
    const UNITS: [&str; 9] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB", "ZiB", "YiB"];
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

fn is_dot_hidden(path: &Path) -> bool {
    path.file_name().is_some_and(is_dot_hidden_name)
}

fn is_dot_hidden_name(name: &std::ffi::OsStr) -> bool {
    name.to_string_lossy().starts_with('.')
}

fn relative_path_text(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn strip_ascii_case_insensitive_suffix<'a>(value: &'a str, suffix: &str) -> Option<&'a str> {
    let suffix_start = value.len().checked_sub(suffix.len())?;
    let prefix = value.get(..suffix_start)?;
    value
        .get(suffix_start..)?
        .eq_ignore_ascii_case(suffix)
        .then_some(prefix)
}

fn metadata_has_hidden_attribute(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x2 != 0
    }
    #[cfg(not(windows))]
    {
        let _ = metadata;
        false
    }
}

fn has_hidden_attribute(path: &Path) -> bool {
    #[cfg(windows)]
    {
        fs::symlink_metadata(path).is_ok_and(|metadata| metadata_has_hidden_attribute(&metadata))
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        false
    }
}

fn directory_name_matches(name: &str, candidates: &[&str]) -> bool {
    candidates
        .iter()
        .any(|candidate| name.eq_ignore_ascii_case(candidate))
}

fn is_real_directory(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_dir())
}

fn is_real_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_file())
}

fn read_retry_interrupted(reader: &mut impl Read, buffer: &mut [u8]) -> io::Result<usize> {
    loop {
        match reader.read(buffer) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => return result,
        }
    }
}

fn run_with_bounded_stderr(
    command: &mut Command,
) -> io::Result<(std::process::ExitStatus, Vec<u8>)> {
    const STDERR_DETAIL_LIMIT: usize = 64 * 1024;
    const STDERR_TRUNCATION_MARKER: &[u8] = b"\n... stderr truncated ...\n";
    const STDERR_HEAD_LIMIT: usize = (STDERR_DETAIL_LIMIT - STDERR_TRUNCATION_MARKER.len()) / 2;
    const STDERR_TAIL_LIMIT: usize =
        STDERR_DETAIL_LIMIT - STDERR_HEAD_LIMIT - STDERR_TRUNCATION_MARKER.len();
    let mut child = command
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let Some(mut stderr) = child.stderr.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(io::Error::other("could not capture command diagnostics"));
    };
    let mut detail_head = Vec::new();
    let mut detail_tail = VecDeque::new();
    let mut truncated = false;
    let mut buffer = [0; 8192];
    loop {
        let count = match read_retry_interrupted(&mut stderr, &mut buffer) {
            Ok(count) => count,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        if count == 0 {
            break;
        }
        let head_count = (STDERR_HEAD_LIMIT - detail_head.len()).min(count);
        detail_head.extend_from_slice(&buffer[..head_count]);
        let tail = &buffer[head_count..count];
        if !tail.is_empty() {
            let overflow = detail_tail
                .len()
                .saturating_add(tail.len())
                .saturating_sub(STDERR_TAIL_LIMIT);
            for _ in 0..overflow {
                detail_tail.pop_front();
            }
            truncated |= overflow > 0;
            detail_tail.extend(tail.iter().copied());
        }
    }
    if truncated {
        detail_head.extend_from_slice(STDERR_TRUNCATION_MARKER);
    }
    detail_head.extend(detail_tail);
    Ok((child.wait()?, detail_head))
}

fn create_unique_media_temp(parent: &Path, stem: &str, extension: &str) -> Result<PathBuf, String> {
    let mut attempt = 0_u64;
    loop {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let candidate = parent.join(format!(
            ".{stem}.mediaforge-{}-{nonce}-{attempt}.tmp.{extension}",
            std::process::id()
        ));
        attempt = attempt
            .checked_add(1)
            .ok_or_else(|| "could not allocate a unique temporary media name".to_owned())?;
        match entry_state(&candidate) {
            Ok(EntryState::Missing) => return Ok(candidate),
            Ok(_) => continue,
            Err(error) => {
                return Err(format!(
                    "cannot inspect temporary media beside {}: {error}",
                    parent.display()
                ));
            }
        }
    }
}

fn remove_temp_with_context(path: &Path, context: String) -> String {
    match fs::remove_file(path) {
        Ok(()) => context,
        Err(error) if error.kind() == io::ErrorKind::NotFound => context,
        Err(error) => format!(
            "{context}; temporary file remains at {}: {error}",
            path.display()
        ),
    }
}

fn create_unique_temp_file(
    parent: &Path,
    prefix: &str,
    description: &str,
) -> Result<(PathBuf, File), String> {
    let mut attempt = 0_u64;
    loop {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let candidate = parent.join(format!(
            "{prefix}-{}-{nonce}-{attempt}.tmp",
            std::process::id()
        ));
        attempt = attempt
            .checked_add(1)
            .ok_or_else(|| format!("could not allocate a temporary {description} name"))?;
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => return Ok((candidate, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!(
                    "cannot create a temporary {description} in {}: {error}",
                    parent.display()
                ));
            }
        }
    }
}

fn read_up_to_full_or_eof(reader: &mut impl Read, buffer: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        let count = read_retry_interrupted(reader, &mut buffer[filled..])?;
        if count == 0 {
            break;
        }
        filled += count;
    }
    Ok(filled)
}

fn sort_entries(entries: &mut Vec<fs::DirEntry>) {
    let mut keyed = entries
        .drain(..)
        .map(|entry| {
            let lowercase_name = entry.file_name().to_string_lossy().to_lowercase();
            (lowercase_name, entry)
        })
        .collect::<Vec<_>>();
    keyed.sort_unstable_by(|(left_lower, left_entry), (right_lower, right_entry)| {
        left_lower.cmp(right_lower).then_with(|| {
            let left_name = left_entry.file_name();
            let right_name = right_entry.file_name();
            left_name
                .to_string_lossy()
                .cmp(&right_name.to_string_lossy())
        })
    });
    entries.extend(keyed.into_iter().map(|(_, entry)| entry));
}

fn natural_name_cmp(left: &str, right: &str) -> std::cmp::Ordering {
    let left_bytes = left.as_bytes();
    let right_bytes = right.as_bytes();
    let (mut left_index, mut right_index) = (0, 0);
    while left_index < left_bytes.len() && right_index < right_bytes.len() {
        if left_bytes[left_index].is_ascii_digit() && right_bytes[right_index].is_ascii_digit() {
            let (left_start, right_start) = (left_index, right_index);
            while left_index < left_bytes.len() && left_bytes[left_index].is_ascii_digit() {
                left_index += 1;
            }
            while right_index < right_bytes.len() && right_bytes[right_index].is_ascii_digit() {
                right_index += 1;
            }
            let left_run = &left_bytes[left_start..left_index];
            let right_run = &right_bytes[right_start..right_index];
            let left_significant = left_run
                .iter()
                .position(|byte| *byte != b'0')
                .map_or(&left_run[left_run.len()..], |offset| &left_run[offset..]);
            let right_significant = right_run
                .iter()
                .position(|byte| *byte != b'0')
                .map_or(&right_run[right_run.len()..], |offset| &right_run[offset..]);
            let numeric_order = left_significant
                .len()
                .cmp(&right_significant.len())
                .then_with(|| left_significant.cmp(right_significant));
            if numeric_order != std::cmp::Ordering::Equal {
                return numeric_order;
            }
            continue;
        }
        let order = left_bytes[left_index]
            .to_ascii_lowercase()
            .cmp(&right_bytes[right_index].to_ascii_lowercase());
        if order != std::cmp::Ordering::Equal {
            return order;
        }
        left_index += 1;
        right_index += 1;
    }
    let left_tail = &left_bytes[left_index..];
    let right_tail = &right_bytes[right_index..];
    left_tail
        .len()
        .cmp(&right_tail.len())
        .then_with(|| {
            left_tail
                .iter()
                .map(u8::to_ascii_lowercase)
                .cmp(right_tail.iter().map(u8::to_ascii_lowercase))
        })
        .then_with(|| left.cmp(right))
}

struct ScanWalkConfig<'a> {
    root: &'a Path,
    include_hidden: bool,
    skip_dirs: &'a [&'a str],
    ignored: &'a [IgnoreRule],
    filters: &'a ScanTraversalFilters<'a>,
    strict: bool,
    record_consumer: Option<ScanRecordConsumer<'a>>,
}

struct ScanWalkResult {
    records: Vec<ScanFileRecord>,
    file_count: usize,
    skipped_paths: usize,
    skipped_examples: Vec<String>,
}

fn walk_with_skip_dirs_mode_counted(config: ScanWalkConfig<'_>) -> Result<ScanWalkResult, String> {
    let ScanWalkConfig {
        root,
        include_hidden,
        skip_dirs,
        ignored,
        filters,
        strict,
        mut record_consumer,
    } = config;
    let mut records = Vec::new();
    let mut file_count = 0_usize;
    let mut skipped_paths = 0_usize;
    let mut skipped_examples = Vec::new();
    let mut pending = vec![(
        root.to_path_buf(),
        Arc::<[IgnoreRule]>::from(ignored),
        filters.path_contains_lower.is_empty(),
    )];
    let mut lowercase_scratch = String::new();
    while let Some((directory, active_ignored, path_filter_satisfied)) = pending.pop() {
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(_) if !strict => {
                record_skipped_path(&directory, &mut skipped_paths, &mut skipped_examples);
                continue;
            }
            Err(error) => {
                return Err(format!(
                    "cannot read directory {}: {error}",
                    directory.display()
                ));
            }
        };
        let mut process_entry = |entry: fs::DirEntry,
                                 skipped_paths: &mut usize,
                                 skipped_examples: &mut Vec<String>|
         -> Result<(), String> {
            let name = entry.file_name();
            if !include_hidden && is_dot_hidden_name(&name) {
                return Ok(());
            }
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(_) if !strict => {
                    record_skipped_path(&directory.join(&name), skipped_paths, skipped_examples);
                    return Ok(());
                }
                Err(error) => {
                    return Err(format!(
                        "cannot inspect {}: {error}",
                        directory.join(&name).display()
                    ));
                }
            };
            if file_type.is_symlink() {
                return Ok(());
            }
            if file_type.is_dir() {
                let name = name.to_string_lossy();
                if directory_name_matches(&name, skip_dirs) {
                    return Ok(());
                }
            }
            let mut extension_value = None;
            let mut kind_value = None;
            if file_type.is_file() {
                let extension = Path::new(&name)
                    .extension()
                    .and_then(|value| value.to_str());
                let kind = classify_extension(extension);
                if filters.kind.is_some_and(|filter| filter != kind)
                    || filters
                        .extensions
                        .is_some_and(|extensions| !matches_extension_filter(extension, extensions))
                {
                    return Ok(());
                }
                extension_value = extension;
                kind_value = Some(kind);
            }
            let path = directory.join(&name);
            if file_type.is_dir() && !include_hidden && has_hidden_attribute(&path) {
                return Ok(());
            }
            let needs_relative_lower = !filters.exclude_path_contains_lower.is_empty()
                || (!path_filter_satisfied && !filters.path_contains_lower.is_empty());
            let needs_relative_path =
                needs_relative_lower || (file_type.is_dir() && !active_ignored.is_empty());
            let mut relative = needs_relative_path.then(|| {
                path.strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/")
            });
            let relative_lower = needs_relative_lower.then(|| {
                lowercase_into(
                    relative.as_ref().expect("filtered paths are materialized"),
                    &mut lowercase_scratch,
                );
                lowercase_scratch.as_str()
            });
            if file_type.is_dir() {
                if !active_ignored.is_empty()
                    && is_ignored_directory(
                        relative
                            .as_deref()
                            .expect("ignored directories are materialized"),
                        &active_ignored,
                    )
                {
                    return Ok(());
                }
                let child_path_filter_satisfied = path_filter_satisfied
                    || filters.path_contains_lower.iter().all(|needle| {
                        relative_lower
                            .as_ref()
                            .is_some_and(|value| value.contains(needle))
                    });
                if filters.exclude_path_contains_lower.iter().any(|needle| {
                    relative_lower
                        .as_ref()
                        .is_some_and(|value| value.contains(needle))
                }) {
                    return Ok(());
                }
                let child_rules = extend_ignore_rules(root, &path, active_ignored.as_ref())?;
                let child_ignored = match child_rules {
                    Cow::Borrowed(_) => Arc::clone(&active_ignored),
                    Cow::Owned(rules) => Arc::from(rules),
                };
                pending.push((path, child_ignored, child_path_filter_satisfied));
            } else if file_type.is_file() {
                if filters.exclude_path_contains_lower.iter().any(|needle| {
                    relative_lower
                        .as_ref()
                        .is_some_and(|value| value.contains(needle))
                }) || (!path_filter_satisfied
                    && !filters.path_contains_lower.is_empty()
                    && !filters.path_contains_lower.iter().all(|needle| {
                        relative_lower
                            .as_ref()
                            .is_some_and(|value| value.contains(needle))
                    }))
                {
                    return Ok(());
                }
                let kind = kind_value.expect("file types are classified before path creation");
                if !active_ignored.is_empty() {
                    let relative = relative.get_or_insert_with(|| {
                        path.strip_prefix(root)
                            .unwrap_or(&path)
                            .to_string_lossy()
                            .replace('\\', "/")
                    });
                    if is_ignored(relative, &active_ignored) {
                        return Ok(());
                    }
                }
                let metadata = match entry.metadata() {
                    Ok(metadata) => metadata,
                    Err(_) if !strict => {
                        record_skipped_path(&path, skipped_paths, skipped_examples);
                        return Ok(());
                    }
                    Err(error) => {
                        return Err(format!("cannot inspect {}: {error}", path.display()));
                    }
                };
                if !include_hidden && metadata_has_hidden_attribute(&metadata) {
                    return Ok(());
                }
                let size = metadata.len();
                if filters.min_size.is_some_and(|minimum| size < minimum)
                    || filters.max_size.is_some_and(|maximum| size > maximum)
                {
                    return Ok(());
                }
                let relative = relative.take().unwrap_or_else(|| {
                    path.strip_prefix(root)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .replace('\\', "/")
                });
                let extension = extension_value.map_or_else(
                    || "[no extension]".to_owned(),
                    |value| {
                        let mut extension = String::with_capacity(value.len() + 1);
                        extension.push('.');
                        extension.push_str(value);
                        extension.make_ascii_lowercase();
                        extension
                    },
                );
                let needs_modified = filters.modified_since.is_some()
                    || filters.modified_until.is_some()
                    || filters.include_modified;
                let modified = if needs_modified {
                    metadata.modified().ok()
                } else {
                    None
                };
                if filters.modified_since.is_some() || filters.modified_until.is_some() {
                    let Some(modified_seconds) = modified
                        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|duration| duration.as_secs())
                    else {
                        return Ok(());
                    };
                    if filters
                        .modified_since
                        .is_some_and(|since| modified_seconds < since)
                        || filters
                            .modified_until
                            .is_some_and(|until| modified_seconds > until)
                    {
                        return Ok(());
                    }
                }
                let record = ScanFileRecord {
                    relative,
                    kind,
                    modified: if filters.include_modified {
                        modified
                    } else {
                        None
                    },
                    size,
                    extension,
                };
                file_count = file_count.saturating_add(1);
                match record_consumer.as_mut() {
                    Some(ScanRecordConsumer::Csv(writer)) => write_csv_record(*writer, &record)
                        .map_err(|error| format!("could not write CSV output: {error}"))?,
                    Some(ScanRecordConsumer::Json(summary)) => summary.add(&record),
                    None => records.push(record),
                }
            }
            Ok(())
        };
        if filters.sort_entries_per_directory {
            let mut entries = entries
                .map(|entry| match entry {
                    Ok(entry) => Ok(entry),
                    Err(_) if !strict => {
                        record_skipped_path(&directory, &mut skipped_paths, &mut skipped_examples);
                        Err(None)
                    }
                    Err(error) => Err(Some(error)),
                })
                .filter_map(|entry| match entry {
                    Ok(entry) => Some(Ok(entry)),
                    Err(None) => None,
                    Err(Some(error)) => Some(Err(error)),
                })
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| {
                    format!("cannot read entry in {}: {error}", directory.display())
                })?;
            sort_entries(&mut entries);
            for entry in entries {
                process_entry(entry, &mut skipped_paths, &mut skipped_examples)?;
            }
        } else {
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(_) if !strict => {
                        record_skipped_path(&directory, &mut skipped_paths, &mut skipped_examples);
                        continue;
                    }
                    Err(error) => {
                        return Err(format!(
                            "cannot read entry in {}: {error}",
                            directory.display()
                        ));
                    }
                };
                process_entry(entry, &mut skipped_paths, &mut skipped_examples)?;
            }
        }
    }
    Ok(ScanWalkResult {
        records,
        file_count,
        skipped_paths,
        skipped_examples,
    })
}

fn record_skipped_path(path: &Path, count: &mut usize, examples: &mut Vec<String>) {
    const MAX_EXAMPLES: usize = 100;
    *count = count.saturating_add(1);
    if examples.len() < MAX_EXAMPLES {
        examples.push(path.to_string_lossy().into_owned());
    }
}

#[derive(Default)]
struct StrictWalkFilters<'a> {
    kinds: Option<&'a [Kind]>,
    extensions: Option<&'a HashSet<String>>,
    sort_entries_per_directory: bool,
    min_size: Option<u64>,
    max_size: Option<u64>,
    excluded_subtree: Option<&'a Path>,
    exclude_path_contains_lower: Option<&'a [String]>,
    omit_absolute_path: bool,
    store_relative_path: bool,
    omit_modified_time: bool,
    omit_extension: bool,
}

fn walk_strict_filtered(
    root: &Path,
    current: &Path,
    include_hidden: bool,
    skip_dirs: &[&str],
    ignored: &[IgnoreRule],
    filters: &StrictWalkFilters<'_>,
    records: &mut Vec<FileRecord>,
) -> Result<(), String> {
    walk_strict_filtered_with_callback(
        root,
        current,
        include_hidden,
        skip_dirs,
        ignored,
        filters,
        |record| {
            records.push(record);
            Ok(())
        },
    )
}

fn walk_strict_paths(
    root: &Path,
    current: &Path,
    include_hidden: bool,
    skip_dirs: &[&str],
    ignored: &[IgnoreRule],
    filters: &StrictWalkFilters<'_>,
    records: &mut Vec<FilePathRecord>,
) -> Result<(), String> {
    walk_strict_filtered_with_callback(
        root,
        current,
        include_hidden,
        skip_dirs,
        ignored,
        filters,
        |record| {
            records.push(FilePathRecord {
                relative: record.relative,
                path: record.path,
            });
            Ok(())
        },
    )
}

fn walk_strict_filtered_with_callback<F>(
    root: &Path,
    current: &Path,
    include_hidden: bool,
    skip_dirs: &[&str],
    ignored: &[IgnoreRule],
    filters: &StrictWalkFilters<'_>,
    mut on_record: F,
) -> Result<(), String>
where
    F: FnMut(FileRecord) -> Result<(), String>,
{
    let mut pending = vec![(current.to_path_buf(), Arc::<[IgnoreRule]>::from(ignored))];
    let mut lowercase_scratch = String::new();
    while let Some((directory, active_ignored)) = pending.pop() {
        let entries = fs::read_dir(&directory)
            .map_err(|error| format!("cannot read directory {}: {error}", directory.display()))?;
        let mut process_entry = |entry: fs::DirEntry| -> Result<(), String> {
            let name = entry.file_name();
            if !include_hidden && is_dot_hidden_name(&name) {
                return Ok(());
            }
            let file_type = entry.file_type().map_err(|error| {
                format!(
                    "cannot inspect {}: {error}",
                    directory.join(&name).display()
                )
            })?;
            if file_type.is_symlink() {
                return Ok(());
            }
            let mut relative = None;
            if file_type.is_dir() {
                let name = name.to_string_lossy();
                if directory_name_matches(&name, skip_dirs) {
                    return Ok(());
                }
            }
            let mut extension_value = None;
            let mut kind_value = None;
            if file_type.is_file() {
                let extension = Path::new(&name)
                    .extension()
                    .and_then(|value| value.to_str());
                if filters
                    .extensions
                    .is_some_and(|values| !matches_extension_filter(extension, values))
                {
                    return Ok(());
                }
                let kind = classify_extension(extension);
                if filters.kinds.is_some_and(|values| !values.contains(&kind)) {
                    return Ok(());
                }
                extension_value = extension;
                kind_value = Some(kind);
            }
            let path = directory.join(&name);
            if file_type.is_dir()
                && ((!include_hidden && has_hidden_attribute(&path))
                    || filters
                        .excluded_subtree
                        .is_some_and(|excluded| path.starts_with(excluded)))
            {
                return Ok(());
            }
            if file_type.is_dir() {
                let has_path_filter = filters
                    .exclude_path_contains_lower
                    .is_some_and(|values| !values.is_empty());
                if !active_ignored.is_empty() || has_path_filter {
                    let relative = relative.get_or_insert(
                        path.strip_prefix(root)
                            .map_err(|error| {
                                format!("cannot relativize {}: {error}", path.display())
                            })?
                            .to_string_lossy()
                            .replace('\\', "/"),
                    );
                    if is_ignored_directory(relative, &active_ignored)
                        || filters.exclude_path_contains_lower.is_some_and(|values| {
                            if values.is_empty() {
                                return false;
                            }
                            lowercase_into(relative, &mut lowercase_scratch);
                            values.iter().any(|value| lowercase_scratch.contains(value))
                        })
                    {
                        return Ok(());
                    }
                }
                let child_rules = extend_ignore_rules(root, &path, active_ignored.as_ref())?;
                let child_ignored = match child_rules {
                    Cow::Borrowed(_) => Arc::clone(&active_ignored),
                    Cow::Owned(rules) => Arc::from(rules),
                };
                pending.push((path, child_ignored));
            } else if file_type.is_file() {
                let kind = kind_value.expect("file types are classified before path creation");
                let has_path_filter = filters
                    .exclude_path_contains_lower
                    .is_some_and(|values| !values.is_empty());
                if !active_ignored.is_empty() || has_path_filter {
                    let relative = relative.get_or_insert(
                        path.strip_prefix(root)
                            .map_err(|error| {
                                format!("cannot relativize {}: {error}", path.display())
                            })?
                            .to_string_lossy()
                            .replace('\\', "/"),
                    );
                    if is_ignored(relative, &active_ignored)
                        || filters.exclude_path_contains_lower.is_some_and(|values| {
                            if values.is_empty() {
                                return false;
                            }
                            lowercase_into(relative, &mut lowercase_scratch);
                            values.iter().any(|value| lowercase_scratch.contains(value))
                        })
                    {
                        return Ok(());
                    }
                }
                let metadata = entry
                    .metadata()
                    .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
                if !include_hidden && metadata_has_hidden_attribute(&metadata) {
                    return Ok(());
                }
                let size = metadata.len();
                if filters.min_size.is_some_and(|minimum| size < minimum)
                    || filters.max_size.is_some_and(|maximum| size > maximum)
                {
                    return Ok(());
                }
                let relative = match relative.take() {
                    Some(relative) => relative,
                    None => path
                        .strip_prefix(root)
                        .map_err(|error| format!("cannot relativize {}: {error}", path.display()))?
                        .to_string_lossy()
                        .replace('\\', "/"),
                };
                let extension = if filters.omit_extension {
                    String::new()
                } else {
                    extension_value
                        .map(|value| format!(".{}", value.to_ascii_lowercase()))
                        .unwrap_or_else(|| "[no extension]".to_owned())
                };
                let record = FileRecord {
                    relative,
                    path: if filters.omit_absolute_path {
                        PathBuf::new()
                    } else if filters.store_relative_path {
                        path.strip_prefix(root)
                            .map_err(|error| {
                                format!("cannot relativize {}: {error}", path.display())
                            })?
                            .to_path_buf()
                    } else {
                        path
                    },
                    size,
                    modified: if filters.omit_modified_time {
                        None
                    } else {
                        metadata.modified().ok()
                    },
                    kind,
                    extension,
                };
                on_record(record)?;
            }
            Ok(())
        };
        if filters.sort_entries_per_directory {
            let mut entries = entries
                .map(|entry| {
                    entry.map_err(|error| {
                        format!("cannot read entry in {}: {error}", directory.display())
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            sort_entries(&mut entries);
            for entry in entries {
                process_entry(entry)?;
            }
        } else {
            for entry in entries {
                let entry = entry.map_err(|error| {
                    format!("cannot read entry in {}: {error}", directory.display())
                })?;
                process_entry(entry)?;
            }
        }
    }
    Ok(())
}

#[derive(Clone, Debug)]
struct IgnoreRule {
    pattern: IgnorePattern,
    base: Arc<str>,
    scan_prefix: Arc<str>,
    negated: bool,
    anchored: bool,
    directory_only: bool,
    has_slash: bool,
}

struct GitIgnoreScope {
    repository_root: Option<PathBuf>,
    scan_prefix: Arc<str>,
    repository_ignore: Option<PathBuf>,
}

#[derive(Clone, Debug)]
enum IgnorePattern {
    Literal(String),
    Glob(Vec<GlobToken>),
}

#[derive(Clone, Debug)]
enum GlobToken {
    Literal(char),
    Any,
    Star { crosses_slash: bool },
    AnyDirectories,
    Class { negated: bool, members: Vec<char> },
}

#[derive(Default)]
struct GlobMatchScratch {
    value: Vec<char>,
    previous: Vec<bool>,
    current: Vec<bool>,
    directory_prefix: Vec<bool>,
}

thread_local! {
    static GLOB_MATCH_SCRATCH: std::cell::RefCell<GlobMatchScratch> =
        std::cell::RefCell::new(GlobMatchScratch::default());
}

fn ignored_by_git(root: &Path) -> Result<Vec<IgnoreRule>, String> {
    let GitIgnoreScope {
        repository_root,
        scan_prefix,
        repository_ignore,
    } = git_repository_scope(root)?;
    let mut global_rules = Vec::new();
    if let Some(path) = global_ignore_file(root)? {
        let metadata = match fs::metadata(&path) {
            Ok(metadata) => Some(metadata),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(format!(
                    "cannot inspect global ignore file {}: {error}",
                    path.display()
                ));
            }
        };
        if metadata.is_some_and(|metadata| metadata.is_file()) {
            let contents = fs::read_to_string(&path).map_err(|error| {
                format!("cannot read global ignore file {}: {error}", path.display())
            })?;
            global_rules.extend(parse_scoped_ignore_rules(&contents, "", &scan_prefix));
        }
    }
    if let Some(path) = repository_ignore {
        let contents = match fs::read_to_string(&path) {
            Ok(contents) => Some(contents),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(format!(
                    "cannot read repository ignore file {}: {error}",
                    path.display()
                ));
            }
        };
        if let Some(contents) = contents {
            global_rules.extend(parse_scoped_ignore_rules(&contents, "", &scan_prefix));
        }
    }
    if let Some(repository_root) = repository_root {
        let mut ancestors = Vec::new();
        let mut directory = root.parent();
        while let Some(current) = directory {
            if !current.starts_with(&repository_root) {
                break;
            }
            ancestors.push(current.to_path_buf());
            if current == repository_root {
                break;
            }
            directory = current.parent();
        }
        ancestors.reverse();
        for directory in ancestors {
            global_rules = extend_ignore_rules_with_prefix(
                &repository_root,
                &directory,
                &global_rules,
                &scan_prefix,
            )?
            .into_owned();
        }
    }
    if !scan_prefix.is_empty() && global_rules.is_empty() {
        global_rules.push(IgnoreRule {
            pattern: IgnorePattern::Literal(String::new()),
            base: Arc::from("\0"),
            scan_prefix: Arc::clone(&scan_prefix),
            negated: false,
            anchored: true,
            directory_only: false,
            has_slash: false,
        });
    }
    Ok(extend_ignore_rules(root, root, &global_rules)?.into_owned())
}

fn git_repository_scope(root: &Path) -> Result<GitIgnoreScope, String> {
    let output = match Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "rev-parse",
            "--show-toplevel",
            "--show-prefix",
            "--git-path",
            "info/exclude",
        ])
        .output()
    {
        Ok(output) if output.status.success() => output,
        Ok(_) => {
            return Ok(GitIgnoreScope {
                repository_root: None,
                scan_prefix: Arc::from(""),
                repository_ignore: None,
            });
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(GitIgnoreScope {
                repository_root: None,
                scan_prefix: Arc::from(""),
                repository_ignore: None,
            });
        }
        Err(error) => return Err(format!("could not resolve Git repository root: {error}")),
    };
    let output = String::from_utf8_lossy(&output.stdout);
    let mut lines = output.lines();
    let repository_root = lines
        .next()
        .filter(|line| !line.trim().is_empty())
        .ok_or_else(|| "Git returned an empty repository root".to_owned())?;
    let prefix = lines.next().unwrap_or_default().trim_matches(['/', '\\']);
    let ignore_path = lines
        .next()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(PathBuf::from);
    let repository_root = fs::canonicalize(repository_root)
        .map_err(|error| format!("cannot access Git repository root {repository_root}: {error}"))?;
    if !root.starts_with(&repository_root) {
        return Err(format!(
            "scan root {} is outside Git repository {}",
            root.display(),
            repository_root.display()
        ));
    }
    let ignore_path = ignore_path.map(|path| {
        if path.is_absolute() {
            path
        } else {
            root.join(path)
        }
    });
    Ok(GitIgnoreScope {
        repository_root: Some(repository_root),
        scan_prefix: Arc::from(prefix),
        repository_ignore: ignore_path,
    })
}

fn parse_scoped_ignore_rules(text: &str, base: &str, scan_prefix: &str) -> Vec<IgnoreRule> {
    text.lines()
        .filter_map(parse_ignore_rule)
        .map(|mut rule| {
            rule.base = Arc::from(base);
            rule.scan_prefix = Arc::from(scan_prefix);
            rule
        })
        .collect()
}

fn global_ignore_file(root: &Path) -> Result<Option<PathBuf>, String> {
    let configured = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["config", "--path", "--get", "core.excludesFile"])
        .output();
    match configured {
        Ok(output) if output.status.success() => {
            let value = String::from_utf8_lossy(&output.stdout);
            let value = value.trim();
            if value.is_empty() {
                return Ok(None);
            }
            let path = PathBuf::from(value);
            return Ok(Some(if path.is_absolute() {
                path
            } else {
                root.join(path)
            }));
        }
        Ok(output) if !output.stderr.is_empty() => {
            return Err(format!(
                "could not resolve Git's global ignore file: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "could not resolve Git's global ignore file: {error}"
            ));
        }
    }

    let config_root = env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("HOME")
                .or_else(|| env::var_os("USERPROFILE"))
                .map(|home| PathBuf::from(home).join(".config"))
        });
    Ok(config_root.map(|path| path.join("git").join("ignore")))
}

fn extend_ignore_rules<'a>(
    root: &Path,
    directory: &Path,
    inherited: &'a [IgnoreRule],
) -> Result<Cow<'a, [IgnoreRule]>, String> {
    let scan_prefix = inherited
        .first()
        .map_or("", |rule| rule.scan_prefix.as_ref());
    extend_ignore_rules_with_prefix(root, directory, inherited, scan_prefix)
}

fn extend_ignore_rules_with_prefix<'a>(
    root: &Path,
    directory: &Path,
    inherited: &'a [IgnoreRule],
    scan_prefix: &str,
) -> Result<Cow<'a, [IgnoreRule]>, String> {
    let path = directory.join(".gitignore");
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(Cow::Borrowed(inherited));
        }
        Err(error) => return Err(format!("cannot inspect {}: {error}", path.display())),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Ok(Cow::Borrowed(inherited));
    }
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
    };
    let local_base = directory
        .strip_prefix(root)
        .map_err(|error| format!("cannot relativize {}: {error}", directory.display()))?
        .to_string_lossy()
        .replace('\\', "/");
    let base = match (scan_prefix.is_empty(), local_base.is_empty()) {
        (true, _) => local_base,
        (false, true) => scan_prefix.to_owned(),
        (false, false) => format!("{scan_prefix}/{local_base}"),
    };
    let base: Arc<str> = Arc::from(base);
    let mut local_rules = Vec::new();
    for mut rule in text.lines().filter_map(parse_ignore_rule) {
        rule.base = Arc::clone(&base);
        rule.scan_prefix = Arc::from(scan_prefix);
        local_rules.push(rule);
    }
    if local_rules.is_empty() {
        return Ok(Cow::Borrowed(inherited));
    }
    let mut rules = inherited.to_vec();
    rules.extend(local_rules);
    Ok(Cow::Owned(rules))
}

fn parse_ignore_rule(line: &str) -> Option<IgnoreRule> {
    let mut pattern = trim_unescaped_trailing_spaces(line.trim_end_matches('\r'));
    if pattern.is_empty() || pattern.starts_with('#') {
        return None;
    }
    let negated = pattern.starts_with('!');
    if negated {
        pattern = &pattern[1..];
    }
    if pattern.is_empty() {
        return None;
    }
    let anchored = pattern.starts_with('/');
    if anchored {
        pattern = &pattern[1..];
    }
    let directory_only = pattern.ends_with('/') && !has_odd_backslashes_before_last(pattern);
    if directory_only {
        pattern = &pattern[..pattern.len() - 1];
    }
    if pattern.is_empty() {
        return None;
    }
    let pattern = parse_glob_pattern(pattern, anchored)?;
    let has_slash = match &pattern {
        IgnorePattern::Literal(value) => value.contains('/'),
        IgnorePattern::Glob(tokens) => tokens
            .iter()
            .any(|token| matches!(token, GlobToken::Literal('/') | GlobToken::AnyDirectories)),
    };
    Some(IgnoreRule {
        pattern,
        base: Arc::from(""),
        scan_prefix: Arc::from(""),
        negated,
        anchored,
        directory_only,
        has_slash,
    })
}

fn trim_unescaped_trailing_spaces(mut pattern: &str) -> &str {
    while pattern.ends_with(' ') && !has_odd_backslashes_before_last(pattern) {
        pattern = &pattern[..pattern.len() - 1];
    }
    pattern
}

fn has_odd_backslashes_before_last(value: &str) -> bool {
    let bytes = value.as_bytes();
    let mut index = bytes.len().saturating_sub(1);
    let mut backslashes = 0;
    while index > 0 && bytes[index - 1] == b'\\' {
        backslashes += 1;
        index -= 1;
    }
    backslashes % 2 == 1
}

fn parse_literal_ignore_pattern(pattern: &str) -> Option<String> {
    let mut literal = String::with_capacity(pattern.len());
    let mut characters = pattern.chars();
    while let Some(character) = characters.next() {
        match character {
            '\\' => literal.push(characters.next()?),
            '*' | '?' | '[' => return None,
            character => literal.push(character),
        }
    }
    Some(literal)
}

fn parse_glob_pattern(pattern: &str, anchored: bool) -> Option<IgnorePattern> {
    if let Some(literal) = parse_literal_ignore_pattern(pattern) {
        return Some(IgnorePattern::Literal(literal));
    }
    let mut tokens = Vec::new();
    let characters: Vec<_> = pattern.chars().collect();
    let mut index = 0;
    while index < characters.len() {
        let character = characters[index];
        match character {
            '\\' => {
                index += 1;
                tokens.push(GlobToken::Literal(*characters.get(index)?));
            }
            '*' => {
                let run_end = (index + 1..characters.len())
                    .find(|next| characters[*next] != '*')
                    .unwrap_or(characters.len());
                let run_length = run_end - index;
                let followed_by_slash = characters.get(run_end) == Some(&'/');
                let preceded_by_slash = index == 0 || characters[index - 1] == '/';
                if run_length == 2 && followed_by_slash && preceded_by_slash {
                    tokens.push(GlobToken::AnyDirectories);
                    index = run_end + 1;
                    continue;
                }
                let trailing_double_star = run_length == 2
                    && run_end == characters.len()
                    && ((index > 0 && characters[index - 1] == '/') || (anchored && index == 0));
                if trailing_double_star {
                    tokens.push(GlobToken::Star {
                        crosses_slash: true,
                    });
                } else {
                    tokens.extend(std::iter::repeat_n(
                        GlobToken::Star {
                            crosses_slash: false,
                        },
                        run_length,
                    ));
                }
                index = run_end;
                continue;
            }
            '?' => tokens.push(GlobToken::Any),
            '[' => {
                let mut class = Vec::new();
                let mut closed = false;
                index += 1;
                while index < characters.len() {
                    match characters[index] {
                        '\\' => {
                            index += 1;
                            class.push(*characters.get(index)?);
                        }
                        ']' if !class.is_empty() => {
                            closed = true;
                            break;
                        }
                        character => class.push(character),
                    }
                    index += 1;
                }
                if closed {
                    tokens.push(GlobToken::Class {
                        negated: class.first().is_some_and(|ch| *ch == '!' || *ch == '^'),
                        members: class,
                    });
                } else {
                    tokens.push(GlobToken::Literal('['));
                    tokens.extend(class.into_iter().map(GlobToken::Literal));
                }
            }
            character => tokens.push(GlobToken::Literal(character)),
        }
        index += 1;
    }
    if tokens
        .iter()
        .all(|token| matches!(token, GlobToken::Literal(_)))
    {
        let mut literal = String::with_capacity(pattern.len());
        for token in tokens {
            let GlobToken::Literal(character) = token else {
                unreachable!("literal-only glob contains a non-literal token");
            };
            literal.push(character);
        }
        Some(IgnorePattern::Literal(literal))
    } else {
        Some(IgnorePattern::Glob(tokens))
    }
}

fn glob_matches(pattern: &IgnorePattern, value: &str, scratch: &mut GlobMatchScratch) -> bool {
    let pattern = match pattern {
        IgnorePattern::Literal(literal) => return literal == value,
        IgnorePattern::Glob(pattern) => pattern,
    };
    if let Some(matched) = match_simple_star_glob(pattern, value) {
        return matched;
    }
    if value.is_ascii() {
        return glob_matches_units(
            pattern,
            value.as_bytes(),
            |byte| byte as char,
            &mut scratch.previous,
            &mut scratch.current,
            &mut scratch.directory_prefix,
        );
    }
    scratch.value.clear();
    scratch.value.extend(value.chars());
    let GlobMatchScratch {
        value,
        previous,
        current,
        directory_prefix,
    } = scratch;
    glob_matches_units(
        pattern,
        value,
        |character| character,
        previous,
        current,
        directory_prefix,
    )
}

fn match_simple_star_glob(pattern: &[GlobToken], value: &str) -> Option<bool> {
    let suffix = matches!(
        pattern.first(),
        Some(GlobToken::Star {
            crosses_slash: false
        })
    )
    .then_some(&pattern[1..])
    .filter(|tokens| {
        tokens
            .iter()
            .all(|token| matches!(token, GlobToken::Literal(character) if *character != '/'))
    });
    if let Some(suffix) = suffix {
        if value.contains('/') {
            return Some(false);
        }
        let mut characters = value.chars().rev();
        return Some(suffix.iter().rev().all(|token| {
            matches!(token, GlobToken::Literal(expected) if Some(*expected) == characters.next())
        }));
    }

    let prefix = matches!(
        pattern.last(),
        Some(GlobToken::Star {
            crosses_slash: false
        })
    )
    .then_some(&pattern[..pattern.len() - 1])
    .filter(|tokens| {
        tokens
            .iter()
            .all(|token| matches!(token, GlobToken::Literal(character) if *character != '/'))
    });
    if let Some(prefix) = prefix {
        if value.contains('/') {
            return Some(false);
        }
        let mut characters = value.chars();
        return Some(prefix.iter().all(|token| {
            matches!(token, GlobToken::Literal(expected) if Some(*expected) == characters.next())
        }));
    }
    None
}

fn glob_matches_path_prefix(
    pattern: &IgnorePattern,
    value: &str,
    scratch: &mut GlobMatchScratch,
) -> bool {
    glob_matches(pattern, value, scratch)
}

fn glob_matches_units<T: Copy>(
    pattern: &[GlobToken],
    value: &[T],
    to_character: impl Fn(T) -> char,
    previous: &mut Vec<bool>,
    current: &mut Vec<bool>,
    directory_prefix: &mut Vec<bool>,
) -> bool {
    let state_len = value.len() + 1;
    previous.resize(state_len, false);
    previous.fill(false);
    current.resize(state_len, false);
    current.fill(false);
    directory_prefix.resize(state_len, false);
    directory_prefix.fill(false);
    previous[0] = true;
    for token in pattern {
        current.fill(false);
        match token {
            GlobToken::Literal(expected) => {
                for index in 1..=value.len() {
                    current[index] =
                        previous[index - 1] && to_character(value[index - 1]) == *expected;
                }
            }
            GlobToken::Any => {
                for index in 1..=value.len() {
                    current[index] = previous[index - 1] && to_character(value[index - 1]) != '/';
                }
            }
            GlobToken::Class { negated, members } => {
                for index in 1..=value.len() {
                    let character = to_character(value[index - 1]);
                    current[index] = previous[index - 1]
                        && character != '/'
                        && character_class_matches(members, *negated, character);
                }
            }
            GlobToken::Star { crosses_slash } => {
                current[0] = previous[0];
                for index in 1..=value.len() {
                    let character = to_character(value[index - 1]);
                    current[index] = previous[index]
                        || ((*crosses_slash || character != '/') && current[index - 1]);
                }
            }
            GlobToken::AnyDirectories => {
                current[0] = previous[0];
                directory_prefix[0] = previous[0];
                for index in 1..=value.len() {
                    directory_prefix[index] = previous[index] || directory_prefix[index - 1];
                    current[index] = previous[index]
                        || (to_character(value[index - 1]) == '/' && directory_prefix[index - 1]);
                }
            }
        }
        std::mem::swap(previous, current);
    }
    previous[value.len()]
}

fn character_class_matches(members: &[char], negated: bool, character: char) -> bool {
    let members = if members.first().is_some_and(|ch| *ch == '!' || *ch == '^') {
        &members[1..]
    } else {
        members
    };
    let mut included = false;
    let mut index = 0;
    while index < members.len() {
        if index + 2 < members.len() && members[index + 1] == '-' {
            included |= members[index] <= character && character <= members[index + 2];
            index += 3;
        } else {
            included |= members[index] == character;
            index += 1;
        }
    }
    included != negated
}

fn rule_matches_path(
    rule: &IgnoreRule,
    relative: &str,
    target_is_directory: bool,
    scratch: &mut GlobMatchScratch,
) -> bool {
    let relative = if rule.base.is_empty() {
        relative
    } else if let Some(relative) = relative
        .strip_prefix(rule.base.as_ref())
        .and_then(|relative| relative.strip_prefix('/'))
    {
        relative
    } else {
        return false;
    };
    if !rule.has_slash && !rule.anchored {
        let mut parts = relative.split('/').peekable();
        return std::iter::from_fn(|| {
            let part = parts.next()?;
            Some((part, parts.peek().is_some() || target_is_directory))
        })
        .any(|(part, is_directory)| {
            (!rule.directory_only || is_directory) && glob_matches(&rule.pattern, part, scratch)
        });
    }
    let mut end = 0;
    let mut parts = relative.split('/').peekable();
    while let Some(part) = parts.next() {
        end += part.len();
        let is_directory = parts.peek().is_some() || target_is_directory;
        if rule.directory_only && !is_directory {
            end += usize::from(parts.peek().is_some());
            continue;
        }
        if glob_matches_path_prefix(&rule.pattern, &relative[..end], scratch) {
            return true;
        }
        end += usize::from(parts.peek().is_some());
    }
    false
}

fn is_ignored(relative: &str, rules: &[IgnoreRule]) -> bool {
    is_ignored_path(relative, false, rules)
}

fn is_ignored_directory(relative: &str, rules: &[IgnoreRule]) -> bool {
    is_ignored_path(relative, true, rules)
}

fn is_ignored_path(relative: &str, is_directory: bool, rules: &[IgnoreRule]) -> bool {
    if rules.is_empty() {
        return false;
    }
    let scan_prefix = rules[0].scan_prefix.as_ref();
    let relative = if scan_prefix.is_empty() {
        Cow::Borrowed(relative)
    } else {
        Cow::Owned(format!("{scan_prefix}/{relative}"))
    };
    GLOB_MATCH_SCRATCH.with(|scratch| {
        let mut scratch = scratch.borrow_mut();
        rules
            .iter()
            .rev()
            .find(|rule| rule_matches_path(rule, &relative, is_directory, &mut scratch))
            .is_some_and(|rule| !rule.negated)
    })
}

fn json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    append_json_string(&mut out, value);
    out
}

fn append_json_string(out: &mut String, value: &str) {
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c <= '\u{1f}' => write!(out, "\\u{:04x}", c as u32)
                .expect("writing JSON escapes to a String cannot fail"),
            c => out.push(c),
        }
    }
    out.push('"');
}

fn write_json_string<W: Write + ?Sized>(output: &mut W, value: &str) -> io::Result<()> {
    output.write_all(b"\"")?;
    let mut segment_start = 0;
    for (index, character) in value.char_indices() {
        let escape = match character {
            '"' => Some(&b"\\\""[..]),
            '\\' => Some(&b"\\\\"[..]),
            '\n' => Some(&b"\\n"[..]),
            '\r' => Some(&b"\\r"[..]),
            '\t' => Some(&b"\\t"[..]),
            character if character <= '\u{1f}' => {
                output.write_all(&value.as_bytes()[segment_start..index])?;
                write!(output, "\\u{:04x}", character as u32)?;
                None
            }
            _ => None,
        };
        if let Some(escape) = escape {
            output.write_all(&value.as_bytes()[segment_start..index])?;
            output.write_all(escape)?;
        }
        if character <= '\u{1f}' || matches!(character, '"' | '\\') {
            segment_start = index + character.len_utf8();
        }
    }
    output.write_all(&value.as_bytes()[segment_start..])?;
    output.write_all(b"\"")
}

fn write_json_stdout(
    description: &str,
    write: impl FnOnce(&mut dyn Write) -> io::Result<()>,
) -> Result<(), String> {
    let stdout = io::stdout();
    let mut output = io::BufWriter::with_capacity(64 * 1024, stdout.lock());
    write(&mut output)
        .and_then(|()| output.flush())
        .map_err(|error| format!("could not write {description}: {error}"))
}

struct JsonWhitespaceWriter<W> {
    inner: W,
    compact: bool,
    in_string: bool,
    escaped: bool,
}

impl<W> JsonWhitespaceWriter<W> {
    fn new(inner: W, compact: bool) -> Self {
        Self {
            inner,
            compact,
            in_string: false,
            escaped: false,
        }
    }
}

impl<W: Write> Write for JsonWhitespaceWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if !self.compact {
            self.inner.write_all(bytes)?;
            return Ok(bytes.len());
        }
        let mut start = 0;
        for (index, byte) in bytes.iter().copied().enumerate() {
            if self.in_string {
                if self.escaped {
                    self.escaped = false;
                } else if byte == b'\\' {
                    self.escaped = true;
                } else if byte == b'"' {
                    self.in_string = false;
                }
            } else if matches!(byte, b' ' | b'\n' | b'\r' | b'\t') {
                self.inner.write_all(&bytes[start..index])?;
                start = index + 1;
            } else if byte == b'"' {
                self.in_string = true;
            }
        }
        self.inner.write_all(&bytes[start..])?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[derive(Clone, Copy)]
struct ScanFilterOptions<'a> {
    kind: Option<Kind>,
    min_size: Option<u64>,
    max_size: Option<u64>,
    modified_since: Option<u64>,
    modified_until: Option<u64>,
    modified_within: Option<u64>,
    path_contains: &'a [String],
    exclude_path_contains: &'a [String],
    extensions: Option<&'a HashSet<String>>,
    include_hidden: bool,
    strict: bool,
}

fn scan_filters_json(filters: ScanFilterOptions<'_>) -> String {
    let ScanFilterOptions {
        kind,
        min_size,
        max_size,
        modified_since,
        modified_until,
        modified_within,
        path_contains,
        exclude_path_contains,
        extensions: extension_filter,
        include_hidden,
        strict,
    } = filters;
    let extensions = extension_filter.map_or_else(
        || "null".to_owned(),
        |extensions| {
            let mut extensions = extensions.iter().collect::<Vec<_>>();
            extensions.sort_unstable();
            format!(
                "[{}]",
                extensions
                    .into_iter()
                    .map(|extension| json_string(extension))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        },
    );
    format!(
        "{{\"type\":{},\"min_size_bytes\":{},\"max_size_bytes\":{},\"modified_since_unix_seconds\":{},\"modified_until_unix_seconds\":{},\"modified_within_seconds\":{},\"path_contains\":[{}],\"exclude_path_contains\":[{}],\"extensions\":{extensions},\"include_hidden\":{include_hidden},\"strict\":{strict}}}",
        kind.map_or("null".to_owned(), |kind| json_string(kind.as_str())),
        min_size.map_or("null".to_owned(), |size| size.to_string()),
        max_size.map_or("null".to_owned(), |size| size.to_string()),
        modified_since.map_or("null".to_owned(), |time| time.to_string()),
        modified_until.map_or("null".to_owned(), |time| time.to_string()),
        modified_within.map_or("null".to_owned(), |seconds| seconds.to_string()),
        path_contains
            .iter()
            .map(|needle| json_string(needle))
            .collect::<Vec<_>>()
            .join(","),
        exclude_path_contains
            .iter()
            .map(|needle| json_string(needle))
            .collect::<Vec<_>>()
            .join(","),
    )
}

struct ScanFilterPreviewOptions<'a> {
    path: &'a Path,
    json: bool,
    compact: bool,
    csv: bool,
    csv_sort: Option<ScanCsvSort>,
    largest_limit: usize,
    largest_limit_default: usize,
    largest_limit_is_explicit: bool,
    filters: ScanFilterOptions<'a>,
}

fn print_scan_filter_preview(options: ScanFilterPreviewOptions<'_>) -> Result<(), String> {
    let ScanFilterPreviewOptions {
        path,
        json,
        compact,
        csv,
        csv_sort,
        largest_limit,
        largest_limit_default,
        largest_limit_is_explicit,
        filters,
    } = options;
    let output_format = if csv {
        "csv"
    } else if json {
        "json"
    } else {
        "text"
    };
    let csv_order = match csv_sort {
        Some(ScanCsvSort::Path) => "path",
        Some(ScanCsvSort::Size) => "size",
        Some(ScanCsvSort::Modified) => "modified",
        None => "scan",
    };
    write_json_stdout("scan filter preview JSON", |output| {
        let mut output = JsonWhitespaceWriter::new(output, compact);
        writeln!(
            &mut output,
            "{{\"schema_version\":1,\"type\":\"scan_filter_preview\",\"scan_performed\":false,\"root\":{},\"output_format\":{},\"csv_order\":{},\"largest_limit\":{},\"largest_limit_default\":{},\"largest_limit_source\":{},\"largest_limit_applies\":{},\"filters\":{}}}",
            json_string(&path.to_string_lossy()),
            json_string(output_format),
            json_string(csv_order),
            largest_limit,
            largest_limit_default,
            json_string(if largest_limit_is_explicit {
                "explicit"
            } else {
                "default"
            }),
            !csv,
            scan_filters_json(filters),
        )
    })
}

fn compare_paths_case_insensitive(left: &str, right: &str) -> std::cmp::Ordering {
    let mut left_bytes = left.bytes();
    let mut right_bytes = right.bytes();
    let case_insensitive = loop {
        match (left_bytes.next(), right_bytes.next()) {
            (Some(left_byte), Some(right_byte))
                if left_byte.is_ascii() && right_byte.is_ascii() =>
            {
                let ordering = left_byte
                    .to_ascii_lowercase()
                    .cmp(&right_byte.to_ascii_lowercase());
                if ordering != std::cmp::Ordering::Equal {
                    break ordering;
                }
            }
            (None, None) => break std::cmp::Ordering::Equal,
            (None, Some(_)) => break std::cmp::Ordering::Less,
            (Some(_), None) => break std::cmp::Ordering::Greater,
            _ => {
                break left
                    .chars()
                    .flat_map(char::to_lowercase)
                    .cmp(right.chars().flat_map(char::to_lowercase));
            }
        }
    };
    case_insensitive.then_with(|| left.cmp(right))
}

fn compare_largest_values(
    left_size: u64,
    left_path: &str,
    right_size: u64,
    right_path: &str,
) -> std::cmp::Ordering {
    std::cmp::Reverse(left_size)
        .cmp(&std::cmp::Reverse(right_size))
        .then_with(|| compare_paths_case_insensitive(left_path, right_path))
}

fn compare_largest_records(left: &ScanFileRecord, right: &ScanFileRecord) -> std::cmp::Ordering {
    compare_largest_values(left.size, &left.relative, right.size, &right.relative)
}

struct LargestRecord<'a>(&'a ScanFileRecord);

impl PartialEq for LargestRecord<'_> {
    fn eq(&self, other: &Self) -> bool {
        compare_largest_records(self.0, other.0).is_eq()
    }
}

impl Eq for LargestRecord<'_> {}

impl PartialOrd for LargestRecord<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for LargestRecord<'_> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        compare_largest_records(self.0, other.0)
    }
}

fn largest_records(records: &[ScanFileRecord], limit: usize) -> Vec<&ScanFileRecord> {
    if limit == 0 {
        return Vec::new();
    }
    if limit >= records.len() {
        let mut ordered = records.iter().collect::<Vec<_>>();
        ordered.sort_unstable_by(|left, right| compare_largest_records(left, right));
        return ordered;
    }
    let mut largest = BinaryHeap::with_capacity(limit);
    for record in records {
        if largest.len() < limit {
            largest.push(LargestRecord(record));
        } else if largest
            .peek()
            .is_some_and(|smallest| compare_largest_records(record, smallest.0).is_lt())
        {
            largest.pop();
            largest.push(LargestRecord(record));
        }
    }
    let mut ordered = largest.into_vec();
    ordered.sort_unstable_by(|left, right| compare_largest_records(left.0, right.0));
    ordered.into_iter().map(|entry| entry.0).collect()
}

fn print_json(
    root: &Path,
    records: &[ScanFileRecord],
    options: ScanJsonOutputOptions,
    skipped_paths: usize,
    skipped_examples: &[String],
    filters: ScanFilterOptions<'_>,
) -> Result<(), String> {
    let ScanJsonOutputOptions {
        largest_limit,
        include_same_size,
        compact,
    } = options;
    let mut by_type: BTreeMap<&str, (u64, u128)> = BTreeMap::new();
    let mut extensions: BTreeMap<&str, (usize, u128)> = BTreeMap::new();
    let mut by_size: HashMap<u64, usize> = HashMap::new();
    let mut seen_sizes = HashSet::new();
    let mut duplicate_sizes = HashSet::new();
    let mut total_bytes = 0_u128;
    for record in records {
        total_bytes += u128::from(record.size);
        let entry = by_type.entry(record.kind.as_str()).or_default();
        entry.0 += 1;
        entry.1 += u128::from(record.size);
        let extension = extensions.entry(&record.extension).or_default();
        extension.0 += 1;
        extension.1 += u128::from(record.size);
        if include_same_size {
            *by_size.entry(record.size).or_default() += 1;
        } else if !seen_sizes.insert(record.size) {
            duplicate_sizes.insert(record.size);
        }
    }
    let largest = largest_records(records, largest_limit);
    let candidate_group_count = if include_same_size {
        by_size.values().filter(|count| **count > 1).count()
    } else {
        duplicate_sizes.len()
    };
    let mut groups = Vec::with_capacity(if include_same_size {
        candidate_group_count
    } else {
        0
    });
    let mut group_indices = HashMap::with_capacity(if include_same_size {
        candidate_group_count
    } else {
        0
    });
    if include_same_size {
        for (size, count) in by_size {
            if count > 1 {
                group_indices.insert(size, groups.len());
                groups.push((size, count, Vec::<&str>::with_capacity(count)));
            }
        }
        for record in records {
            if let Some(index) = group_indices.get(&record.size) {
                groups[*index].2.push(&record.relative);
            }
        }
        groups.sort_unstable_by_key(|(size, _, _)| *size);
        drop(group_indices);
        for (_, _, files) in &mut groups {
            files.sort_unstable_by(|left, right| compare_paths_case_insensitive(left, right));
        }
    }
    let stdout = io::stdout();
    let output = io::BufWriter::with_capacity(64 * 1024, stdout.lock());
    let mut output = JsonWhitespaceWriter::new(output, compact);
    macro_rules! write_scan_json_line {
        ($($argument:tt)*) => {
            writeln!(output, $($argument)*)
                .map_err(|error| format!("could not write scan JSON: {error}"))?;
        };
    }
    write_scan_json_line!("{{");
    write_scan_json_line!("  \"schema_version\": 1,");
    write_scan_json_line!("  \"root\": {},", json_string(&root.to_string_lossy()));
    write_scan_json_line!("  \"files\": {},", records.len());
    write_scan_json_line!("  \"incomplete\": {},", skipped_paths > 0);
    write_scan_json_line!("  \"skipped_paths\": {skipped_paths},");
    write_scan_json_line!(
        "  \"skipped_paths_truncated\": {},",
        skipped_paths > skipped_examples.len()
    );
    write_scan_json_line!(
        "  \"skipped_path_examples\": [{}],",
        skipped_examples
            .iter()
            .map(|path| json_string(path))
            .collect::<Vec<_>>()
            .join(",")
    );
    write_scan_json_line!("  \"filters\": {},", scan_filters_json(filters));
    write_scan_json_line!("  \"largest_limit\": {largest_limit},");
    write_scan_json_line!("  \"same_size_candidates_included\": {include_same_size},");
    write_scan_json_line!("  \"total_bytes\": {},", total_bytes);
    write_scan_json_line!(
        "  \"total_size\": {},",
        json_string(&human_size_wide(total_bytes))
    );
    write_scan_json_line!("  \"by_type\": {{");
    for (index, (kind, (count, bytes))) in by_type.iter().enumerate() {
        let comma = if index + 1 == by_type.len() { "" } else { "," };
        write_scan_json_line!(
            "    {}: {{\"files\": {}, \"bytes\": {}, \"size\": {}}}{}",
            json_string(kind),
            count,
            bytes,
            json_string(&human_size_wide(*bytes)),
            comma
        );
    }
    write_scan_json_line!("  }},\n  \"extensions\": {{");
    let mut ext_sorted: Vec<_> = extensions.iter().collect();
    ext_sorted.sort_unstable_by(|a, b| b.1.0.cmp(&a.1.0).then_with(|| a.0.cmp(b.0)));
    for (index, (extension, (count, _))) in ext_sorted.iter().enumerate() {
        let comma = if index + 1 == ext_sorted.len() {
            ""
        } else {
            ","
        };
        write_scan_json_line!("    {}: {}{}", json_string(extension), count, comma);
    }
    write_scan_json_line!("  }},\n  \"extension_bytes\": {{");
    for (index, (extension, (_, bytes))) in extensions.iter().enumerate() {
        let comma = if index + 1 == extensions.len() {
            ""
        } else {
            ","
        };
        write_scan_json_line!("    {}: {}{}", json_string(extension), bytes, comma);
    }
    write_scan_json_line!("  }},\n  \"largest\": [");
    for (index, record) in largest.iter().enumerate() {
        let comma = if index + 1 == largest.len() { "" } else { "," };
        write_scan_json_line!(
            "    {{\"path\": {}, \"size_bytes\": {}, \"size\": {}, \"type\": {}}}{}",
            json_string(&record.relative),
            record.size,
            json_string(&human_size(record.size)),
            json_string(record.kind.as_str()),
            comma
        );
    }
    if include_same_size {
        write_scan_json_line!("  ],\n  \"same_size_candidates\": [");
        for (index, (size, _, files)) in groups.iter().enumerate() {
            let comma = if index + 1 == groups.len() { "" } else { "," };
            write!(output, "    {{\"size_bytes\": {}, \"files\": [", size)
                .map_err(|error| format!("could not write scan JSON: {error}"))?;
            for (file_index, file) in files.iter().enumerate() {
                if file_index > 0 {
                    output
                        .write_all(b", ")
                        .map_err(|error| format!("could not write scan JSON: {error}"))?;
                }
                write_json_string(&mut output, file)
                    .map_err(|error| format!("could not write scan JSON: {error}"))?;
            }
            writeln!(output, "]}}{}", comma)
                .map_err(|error| format!("could not write scan JSON: {error}"))?;
        }
        write_scan_json_line!("  ],");
    } else {
        write_scan_json_line!("  ],\n  \"same_size_candidates\": null,");
    }
    write_scan_json_line!("  \"same_size_candidate_groups\": {candidate_group_count},");
    write_scan_json_line!(
        "  \"note\": \"Same-size groups are candidates only; byte-for-byte duplicates are not verified.\"\n}}"
    );
    output
        .flush()
        .map_err(|error| format!("could not write scan JSON: {error}"))?;
    Ok(())
}

struct ScanJsonOutputOptions {
    largest_limit: usize,
    include_same_size: bool,
    compact: bool,
}

fn print_json_summary(
    root: &Path,
    summary: &mut ScanJsonAccumulator,
    skipped_paths: usize,
    skipped_examples: &[String],
    filters: ScanFilterOptions<'_>,
    compact: bool,
) -> Result<(), String> {
    let largest = summary.largest_sorted();
    let stdout = io::stdout();
    let output = io::BufWriter::with_capacity(64 * 1024, stdout.lock());
    let mut output = JsonWhitespaceWriter::new(output, compact);
    macro_rules! write_scan_json_line {
        ($($argument:tt)*) => {
            writeln!(output, $($argument)*)
                .map_err(|error| format!("could not write scan JSON: {error}"))?;
        };
    }
    write_scan_json_line!("{{");
    write_scan_json_line!("  \"schema_version\": 1,");
    write_scan_json_line!("  \"root\": {},", json_string(&root.to_string_lossy()));
    write_scan_json_line!("  \"files\": {},", summary.file_count);
    write_scan_json_line!("  \"incomplete\": {},", skipped_paths > 0);
    write_scan_json_line!("  \"skipped_paths\": {skipped_paths},");
    write_scan_json_line!(
        "  \"skipped_paths_truncated\": {},",
        skipped_paths > skipped_examples.len()
    );
    write_scan_json_line!(
        "  \"skipped_path_examples\": [{}],",
        skipped_examples
            .iter()
            .map(|path| json_string(path))
            .collect::<Vec<_>>()
            .join(",")
    );
    write_scan_json_line!("  \"filters\": {},", scan_filters_json(filters));
    write_scan_json_line!("  \"largest_limit\": {},", summary.largest_limit);
    write_scan_json_line!("  \"same_size_candidates_included\": false,");
    write_scan_json_line!("  \"total_bytes\": {},", summary.total_bytes);
    write_scan_json_line!(
        "  \"total_size\": {},",
        json_string(&human_size_wide(summary.total_bytes))
    );
    write_scan_json_line!("  \"by_type\": {{");
    for (index, (kind, (count, bytes))) in summary.by_type.iter().enumerate() {
        let comma = if index + 1 == summary.by_type.len() {
            ""
        } else {
            ","
        };
        write_scan_json_line!(
            "    {}: {{\"files\": {}, \"bytes\": {}, \"size\": {}}}{}",
            json_string(kind),
            count,
            bytes,
            json_string(&human_size_wide(*bytes)),
            comma
        );
    }
    write_scan_json_line!("  }},\n  \"extensions\": {{");
    let mut ext_sorted: Vec<_> = summary.extensions.iter().collect();
    ext_sorted.sort_unstable_by(|a, b| b.1.0.cmp(&a.1.0).then_with(|| a.0.cmp(b.0)));
    for (index, (extension, (count, _))) in ext_sorted.iter().enumerate() {
        let comma = if index + 1 == ext_sorted.len() {
            ""
        } else {
            ","
        };
        write_scan_json_line!("    {}: {}{}", json_string(extension), count, comma);
    }
    write_scan_json_line!("  }},\n  \"extension_bytes\": {{");
    for (index, (extension, (_, bytes))) in summary.extensions.iter().enumerate() {
        let comma = if index + 1 == summary.extensions.len() {
            ""
        } else {
            ","
        };
        write_scan_json_line!("    {}: {}{}", json_string(extension), bytes, comma);
    }
    write_scan_json_line!("  }},\n  \"largest\": [");
    for (index, record) in largest.iter().enumerate() {
        let comma = if index + 1 == largest.len() { "" } else { "," };
        write_scan_json_line!(
            "    {{\"path\": {}, \"size_bytes\": {}, \"size\": {}, \"type\": {}}}{}",
            json_string(&record.relative),
            record.size,
            json_string(&human_size(record.size)),
            json_string(record.kind.as_str()),
            comma
        );
    }
    write_scan_json_line!("  ],\n  \"same_size_candidates\": null,");
    write_scan_json_line!(
        "  \"same_size_candidate_groups\": {},",
        summary.candidate_group_count
    );
    write_scan_json_line!(
        "  \"note\": \"Same-size groups are candidates only; byte-for-byte duplicates are not verified.\"\n}}"
    );
    output
        .flush()
        .map_err(|error| format!("could not write scan JSON: {error}"))?;
    Ok(())
}

fn print_report(
    root: &Path,
    records: &[ScanFileRecord],
    largest_limit: usize,
    skipped_paths: usize,
    skipped_examples: &[String],
) {
    let mut total_bytes = 0_u128;
    let mut by_type: BTreeMap<&str, (u64, u128)> = BTreeMap::new();
    let mut seen_sizes = HashSet::new();
    let mut duplicate_sizes = HashSet::new();
    for record in records {
        total_bytes += u128::from(record.size);
        let entry = by_type.entry(record.kind.as_str()).or_default();
        entry.0 += 1;
        entry.1 += u128::from(record.size);
        if !seen_sizes.insert(record.size) {
            duplicate_sizes.insert(record.size);
        }
    }
    let largest = largest_records(records, largest_limit);
    let candidate_groups = duplicate_sizes.len();
    println!("MediaForge scan: {}", root.display());
    println!("{} files · {}", records.len(), human_size_wide(total_bytes));
    if skipped_paths > 0 {
        println!("Warning: incomplete scan; skipped {skipped_paths} unreadable path(s).");
        for path in skipped_examples {
            println!("  skipped: {path}");
        }
        if skipped_paths > skipped_examples.len() {
            println!("  ... and {} more", skipped_paths - skipped_examples.len());
        }
    }
    println!("\nBy type");
    if by_type.is_empty() {
        println!("  No files found.");
    }
    for (kind, (count, bytes)) in by_type {
        println!(
            "  {kind:<9} {count:>6} files  {:>10}",
            human_size_wide(bytes)
        );
    }
    println!("\nLargest files");
    for record in &largest {
        println!("  {:>10}  {}", human_size(record.size), record.relative);
    }
    println!("\nSame-size candidate groups: {candidate_groups} (not content-verified)");
}

fn write_csv_field(output: &mut (impl Write + ?Sized), value: &str) -> io::Result<()> {
    let bytes = value.as_bytes();
    let mut start = 0;
    output.write_all(b"\"")?;
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'"' {
            output.write_all(&bytes[start..=index])?;
            output.write_all(b"\"")?;
            start = index + 1;
        }
    }
    output.write_all(&bytes[start..])?;
    output.write_all(b"\"")
}

fn write_csv_column(output: &mut impl Write, first: &mut bool, value: &str) -> io::Result<()> {
    if !*first {
        output.write_all(b",")?;
    }
    *first = false;
    write_csv_field(output, value)
}

fn write_csv_header(output: &mut (impl Write + ?Sized)) -> io::Result<()> {
    output.write_all(b"path,type,extension,size_bytes,modified_unix_seconds\n")
}

fn write_csv_record(output: &mut (impl Write + ?Sized), record: &ScanFileRecord) -> io::Result<()> {
    write_csv_field(output, &record.relative)?;
    output.write_all(b",")?;
    write_csv_field(output, record.kind.as_str())?;
    output.write_all(b",")?;
    write_csv_field(output, &record.extension)?;
    write!(output, ",{}", record.size)?;
    let modified = record
        .modified
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs());
    if let Some(modified) = modified {
        writeln!(output, ",{}", modified)
    } else {
        output.write_all(b",\n")
    }
}

fn print_csv(records: &[ScanFileRecord]) -> Result<(), String> {
    let stdout = io::stdout();
    let mut output = io::BufWriter::with_capacity(64 * 1024, stdout.lock());
    let result = (|| -> io::Result<()> {
        write_csv_header(&mut output)?;
        for record in records {
            write_csv_record(&mut output, record)?;
        }
        output.flush()
    })();
    result.map_err(|error| format!("could not write CSV output: {error}"))
}

fn print_scan_csv_info(
    root: &Path,
    files: usize,
    skipped_paths: usize,
    skipped_examples: &[String],
    filters: ScanFilterOptions<'_>,
    csv_sort: Option<ScanCsvSort>,
) {
    let examples = skipped_examples
        .iter()
        .map(|path| json_string(path))
        .collect::<Vec<_>>()
        .join(",");
    let order = match csv_sort {
        Some(ScanCsvSort::Path) => "path",
        Some(ScanCsvSort::Size) => "size",
        Some(ScanCsvSort::Modified) => "modified",
        None => "scan",
    };
    eprintln!(
        "{{\"schema_version\":1,\"type\":\"scan_info\",\"root\":{},\"files\":{files},\"incomplete\":{},\"skipped_paths\":{skipped_paths},\"skipped_paths_truncated\":{},\"skipped_path_examples\":[{examples}],\"filters\":{},\"csv_order\":{}}}",
        json_string(&root.to_string_lossy()),
        skipped_paths > 0,
        skipped_paths > skipped_examples.len(),
        scan_filters_json(filters),
        json_string(order),
    );
}

fn parse_scan_settings(contents: &str, path: &Path) -> Result<Vec<String>, String> {
    let contents = contents.strip_prefix('\u{feff}').unwrap_or(contents);
    let mut args = Vec::new();
    let mut seen_single_value_keys = HashSet::new();
    let mut min_size = None;
    let mut max_size = None;
    let mut modified_since = None;
    let mut modified_until = None;
    let mut modified_within = false;
    for (index, raw_line) in contents.lines().enumerate() {
        let line_number = index + 1;
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line.split_once('=').ok_or_else(|| {
            format!(
                "invalid scan settings at {}:{line_number}; expected key=value",
                path.display()
            )
        })?;
        let key = key.trim();
        let value = value.trim();
        let invalid = || {
            format!(
                "invalid scan settings at {}:{line_number}: {key} requires a value",
                path.display()
            )
        };
        if !matches!(key, "extension" | "path_contains" | "exclude_path_contains")
            && !seen_single_value_keys.insert(key.to_owned())
        {
            return Err(format!(
                "duplicate scan setting '{key}' at {}:{line_number}",
                path.display()
            ));
        }
        match key {
            "type"
            | "min_size"
            | "max_size"
            | "modified_since"
            | "modified_until"
            | "modified_within"
            | "path_contains"
            | "exclude_path_contains"
            | "extension"
            | "largest" => {
                if value.is_empty() {
                    return Err(invalid());
                }
                let invalid_value = |reason: &str| {
                    format!(
                        "invalid scan setting at {}:{line_number}: {key} {reason}",
                        path.display()
                    )
                };
                match key {
                    "type" if parse_asset_kind(value).is_none() => {
                        return Err(invalid_value(
                            "must be video, audio, image, model, texture, project, or other",
                        ));
                    }
                    "min_size" => {
                        min_size = Some((
                            parse_size(value).map_err(|_| invalid_value("has an invalid size"))?,
                            line_number,
                        ));
                    }
                    "max_size" => {
                        max_size = Some((
                            parse_size(value).map_err(|_| invalid_value("has an invalid size"))?,
                            line_number,
                        ));
                    }
                    "modified_since" => {
                        modified_since = Some((
                            value
                                .parse::<u64>()
                                .map_err(|_| invalid_value("must be non-negative Unix seconds"))?,
                            line_number,
                        ));
                    }
                    "modified_until" => {
                        modified_until = Some((
                            value
                                .parse::<u64>()
                                .map_err(|_| invalid_value("must be non-negative Unix seconds"))?,
                            line_number,
                        ));
                    }
                    "modified_within" => {
                        parse_duration_seconds(value).map_err(|error| invalid_value(&error))?;
                        modified_within = true;
                    }
                    "extension" => {
                        parse_extensions(value).map_err(|error| invalid_value(&error))?;
                    }
                    "largest" => {
                        value
                            .parse::<usize>()
                            .map_err(|_| invalid_value("must be a non-negative integer"))?;
                    }
                    _ => {}
                }
                let option = match key {
                    "type" => "--type",
                    "min_size" => "--min-size",
                    "max_size" => "--max-size",
                    "modified_since" => "--modified-since",
                    "modified_until" => "--modified-until",
                    "modified_within" => "--modified-within",
                    "path_contains" => "--path-contains",
                    "exclude_path_contains" => "--exclude-path-contains",
                    "extension" => "--extension",
                    "largest" => "--largest",
                    _ => unreachable!(),
                };
                args.push(option.to_owned());
                args.push(value.to_owned());
            }
            "include_hidden" | "strict" => match value.to_ascii_lowercase().as_str() {
                "true" | "yes" | "1" => args.push(if key == "include_hidden" {
                    "--include-hidden".to_owned()
                } else {
                    "--strict".to_owned()
                }),
                "false" | "no" | "0" => {}
                _ => {
                    return Err(format!(
                        "invalid scan setting at {}:{line_number}: {key} must be true or false",
                        path.display()
                    ));
                }
            },
            _ => {
                return Err(format!(
                    "unknown scan setting '{}' at {}:{line_number}",
                    key,
                    path.display()
                ));
            }
        }
    }
    if let (Some((min, _)), Some((max, line))) = (min_size, max_size)
        && max < min
    {
        return Err(format!(
            "invalid scan settings at {}:{line}: max_size must be greater than or equal to min_size",
            path.display()
        ));
    }
    if let (Some((since, _)), Some((until, line))) = (modified_since, modified_until)
        && until < since
    {
        return Err(format!(
            "invalid scan settings at {}:{line}: modified_until must be greater than or equal to modified_since",
            path.display()
        ));
    }
    if modified_within && (modified_since.is_some() || modified_until.is_some()) {
        return Err(format!(
            "invalid scan settings at {}: modified_within cannot be combined with modified_since or modified_until",
            path.display()
        ));
    }
    Ok(args)
}

fn scan_args_with_settings(args: &[String]) -> Result<Vec<String>, String> {
    let mut settings_path = None;
    let mut command_args = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--settings" {
            if settings_path.is_some() {
                return Err("--settings may only be specified once".to_owned());
            }
            index += 1;
            let value = args
                .get(index)
                .ok_or_else(|| "--settings requires a file path".to_owned())?;
            settings_path = Some(PathBuf::from(value));
        } else {
            command_args.push(arg.clone());
            if matches!(
                arg.as_str(),
                "--type"
                    | "--min-size"
                    | "--max-size"
                    | "--modified-since"
                    | "--modified-until"
                    | "--modified-within"
                    | "--path-contains"
                    | "--exclude-path-contains"
                    | "--extension"
                    | "--largest"
                    | "--sort"
            ) {
                index += 1;
                if let Some(value) = args.get(index) {
                    command_args.push(value.clone());
                }
            }
        }
        index += 1;
    }
    let Some(path) = settings_path else {
        return Ok(command_args);
    };
    let contents = fs::read_to_string(&path)
        .map_err(|error| format!("cannot read scan settings {}: {error}", path.display()))?;
    let mut resolved = parse_scan_settings(&contents, &path)?;
    resolved.extend(command_args);
    Ok(resolved)
}

fn write_new_file_atomically(
    path: &Path,
    contents: &[u8],
    description: &str,
) -> Result<(), String> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    if path.file_name().is_none() {
        return Err(format!("{description} path must name a file"));
    }
    if entry_state(path).map_err(|error| format!("cannot inspect {}: {error}", path.display()))?
        != EntryState::Missing
    {
        return Err(format!(
            "{description} file already exists; refusing to overwrite: {}",
            path.display()
        ));
    }
    let (temporary, mut file) = create_unique_temp_file(parent, ".mediaforge-config", description)
        .map_err(|error| format!("{error} beside {}", path.display()))?;
    let write_result = file.write_all(contents).and_then(|()| file.sync_all());
    drop(file);
    if let Err(error) = write_result {
        let cleanup = fs::remove_file(&temporary);
        return Err(match cleanup {
            Ok(()) => format!("could not write {description}: {error}"),
            Err(cleanup_error) if cleanup_error.kind() == io::ErrorKind::NotFound => {
                format!("could not write {description}: {error}")
            }
            Err(cleanup_error) => format!(
                "could not write {description}: {error}; temporary file remains at {}: {cleanup_error}",
                temporary.display()
            ),
        });
    }
    if let Err(error) = move_file_no_replace(&temporary, path) {
        let publish_error = if error.kind() == io::ErrorKind::AlreadyExists {
            format!(
                "{description} file already exists; refusing to overwrite: {}",
                path.display()
            )
        } else {
            format!("could not publish {}: {error}", path.display())
        };
        let cleanup = fs::remove_file(&temporary);
        return Err(match cleanup {
            Ok(()) => publish_error,
            Err(cleanup_error) if cleanup_error.kind() == io::ErrorKind::NotFound => publish_error,
            Err(cleanup_error) => format!(
                "{publish_error}; temporary file remains at {}: {cleanup_error}",
                temporary.display()
            ),
        });
    }
    Ok(())
}

fn run_scan_config(args: &[String]) -> Result<(), String> {
    let Some(action) = args.first().map(String::as_str) else {
        println!(
            "Usage: mediaforge scan-config <init|validate|show> [FILE] [--json]\n\nCreate, validate, or inspect reusable scan filter settings."
        );
        return Ok(());
    };
    if action == "--help" || action == "-h" {
        println!(
            "Usage: mediaforge scan-config <init|validate|show> [FILE] [--json]\n\nCreate, validate, or inspect reusable scan filter settings."
        );
        return Ok(());
    }
    if action == "validate" {
        if args.iter().any(|arg| arg == "--help" || arg == "-h") {
            println!(
                "Usage: mediaforge scan-config validate [FILE]\n\nValidate scan filter keys, values, and cross-field constraints without scanning files. Defaults to .mediaforge-scan."
            );
            return Ok(());
        }
        if args.len() > 2 {
            return Err("scan-config validate accepts one file path".to_owned());
        }
        let path = args
            .get(1)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(".mediaforge-scan"));
        let contents = fs::read_to_string(&path)
            .map_err(|error| format!("cannot read scan settings {}: {error}", path.display()))?;
        parse_scan_settings(&contents, &path)?;
        println!("Valid scan settings {}", path.display());
        return Ok(());
    }
    if action == "show" {
        if args.iter().any(|arg| arg == "--help" || arg == "-h") {
            println!(
                "Usage: mediaforge scan-config show [FILE] [--json]\n\nShow active key=value filters from a validated settings file. --json emits a machine-readable settings array. Command-line scan options may override scalar values. Defaults to .mediaforge-scan."
            );
            return Ok(());
        }
        let mut settings_path = None;
        let mut json = false;
        for arg in &args[1..] {
            if arg == "--json" {
                json = true;
            } else if settings_path.is_none() {
                settings_path = Some(PathBuf::from(arg));
            } else {
                return Err("scan-config show accepts one file path and --json".to_owned());
            }
        }
        let path = settings_path.unwrap_or_else(|| PathBuf::from(".mediaforge-scan"));
        let contents = fs::read_to_string(&path)
            .map_err(|error| format!("cannot read scan settings {}: {error}", path.display()))?;
        parse_scan_settings(&contents, &path)?;
        if json {
            write_json_stdout("scan settings JSON", |output| {
                write!(
                    output,
                    "{{\"schema_version\":1,\"type\":\"scan_settings\",\"path\":"
                )?;
                write_json_string(output, &path.to_string_lossy())?;
                output.write_all(b",\"settings\":[")?;
                let mut first = true;
                for line in contents
                    .strip_prefix('\u{feff}')
                    .unwrap_or(&contents)
                    .lines()
                {
                    let line = line.trim();
                    if line.is_empty() || line.starts_with('#') {
                        continue;
                    }
                    let (key, value) = line.split_once('=').expect("settings were validated");
                    if !first {
                        output.write_all(b",")?;
                    }
                    first = false;
                    output.write_all(b"{\"key\":")?;
                    write_json_string(output, key.trim())?;
                    output.write_all(b",\"value\":")?;
                    write_json_string(output, value.trim())?;
                    output.write_all(b"}")?;
                }
                output.write_all(b"]}\n")
            })?;
            return Ok(());
        }
        println!("Active scan settings from {}", path.display());
        let mut count = 0;
        for line in contents
            .strip_prefix('\u{feff}')
            .unwrap_or(&contents)
            .lines()
        {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            println!("  {line}");
            count += 1;
        }
        if count == 0 {
            println!("  (no active filters)");
        }
        return Ok(());
    }
    if action != "init" {
        return Err(format!(
            "unknown scan-config action '{action}'; use init, validate, or show"
        ));
    }
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!(
            "Usage: mediaforge scan-config init [FILE]\n\nCreate a reusable scan filter settings file without overwriting existing files. Defaults to .mediaforge-scan in the current directory."
        );
        return Ok(());
    }
    if args.len() > 2 {
        return Err("scan-config init accepts one file path".to_owned());
    }
    let path = args
        .get(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(".mediaforge-scan"));
    if path.file_name().is_none() {
        return Err("settings path must name a file".to_owned());
    }
    let contents = "# MediaForge scan filters; remove # to enable a setting\n\
# type=video\n\
# min_size=500M\n\
# max_size=2G\n\
# modified_within=30d\n\
# extension=mov,mxf\n\
# extension=mp4\n\
# path_contains=shot\n\
# exclude_path_contains=proxy\n\
# include_hidden=false\n\
# strict=false\n\
# largest=50\n";
    write_new_file_atomically(&path, contents.as_bytes(), "scan settings")?;
    println!("Created scan settings {}", path.display());
    Ok(())
}

fn files_identical(
    left: &Path,
    right: &Path,
    size: u64,
    left_buffer: &mut Vec<u8>,
    right_buffer: &mut Vec<u8>,
) -> io::Result<bool> {
    const DEFAULT_BUFFER_SIZE: usize = 8 * 1024;
    const MAX_BUFFER_SIZE: usize = 1024 * 1024;
    let capacity = usize::try_from(size)
        .unwrap_or(MAX_BUFFER_SIZE)
        .clamp(DEFAULT_BUFFER_SIZE, MAX_BUFFER_SIZE);
    let mut left = fs::File::open(left)?;
    let mut right = fs::File::open(right)?;
    let left_before = left.metadata()?;
    let right_before = right.metadata()?;
    if left_before.len() != size || right_before.len() != size {
        return Ok(false);
    }
    let left_modified_before = left_before.modified().ok();
    let right_modified_before = right_before.modified().ok();
    left_buffer.resize(capacity, 0);
    right_buffer.resize(capacity, 0);
    let mut remaining = size;
    loop {
        if remaining == 0 {
            let mut left_extra = [0_u8; 1];
            let mut right_extra = [0_u8; 1];
            if read_retry_interrupted(&mut left, &mut left_extra)? != 0
                || read_retry_interrupted(&mut right, &mut right_extra)? != 0
            {
                return Ok(false);
            }
            let left_after = left.metadata()?;
            let right_after = right.metadata()?;
            return Ok(left_after.len() == size
                && right_after.len() == size
                && left_after.modified().ok() == left_modified_before
                && right_after.modified().ok() == right_modified_before);
        }
        let requested = remaining.min(capacity as u64) as usize;
        let left_count = read_up_to_full_or_eof(&mut left, &mut left_buffer[..requested])?;
        let right_count = read_up_to_full_or_eof(&mut right, &mut right_buffer[..requested])?;
        if left_count != requested
            || right_count != requested
            || left_buffer[..left_count] != right_buffer[..right_count]
        {
            return Ok(false);
        }
        remaining = remaining.saturating_sub(left_count as u64);
    }
}

fn sample_signature(path: &Path, size: u64) -> io::Result<u64> {
    const SAMPLE_SIZE: usize = 4096;
    let mut file = fs::File::open(path)?;
    let mut offsets = [0_u64; 3];
    let mut offset_count = 1;
    if size > SAMPLE_SIZE as u64 {
        offsets[1] = size / 2 - (SAMPLE_SIZE as u64 / 2);
        offsets[2] = size.saturating_sub(SAMPLE_SIZE as u64);
        offset_count = 3;
    }
    offsets[..offset_count].sort_unstable();
    let mut unique_count = 0;
    for index in 0..offset_count {
        if unique_count == 0 || offsets[index] != offsets[unique_count - 1] {
            offsets[unique_count] = offsets[index];
            unique_count += 1;
        }
    }
    let mut buffer = [0_u8; SAMPLE_SIZE];
    let mut hasher = DefaultHasher::new();
    size.hash(&mut hasher);
    for offset in offsets[..unique_count].iter().copied() {
        file.seek(io::SeekFrom::Start(offset))?;
        let count = (size - offset).min(SAMPLE_SIZE as u64) as usize;
        file.read_exact(&mut buffer[..count])?;
        offset.hash(&mut hasher);
        count.hash(&mut hasher);
        buffer[..count].hash(&mut hasher);
    }
    Ok(hasher.finish())
}

fn sample_signature_for_path(root: &Path, relative: &str, size: u64) -> Result<u64, String> {
    let path = root.join(relative);
    sample_signature(&path, size)
        .map_err(|error| format!("could not sample {}: {error}", path.display()))
}

fn for_each_sample_signature<P, F>(
    root: &Path,
    paths: &[P],
    size: u64,
    jobs: usize,
    mut visit: F,
) -> Result<(), String>
where
    P: AsRef<str> + Sync,
    F: FnMut(usize, Result<u64, String>) -> Result<(), String>,
{
    const CHUNK_SIZE: usize = 256;
    type SampleChunk = (usize, Vec<Result<u64, String>>);
    if jobs == 1 || paths.len() < 8 || paths.len() <= CHUNK_SIZE {
        for (index, relative) in paths.iter().enumerate() {
            visit(
                index,
                sample_signature_for_path(root, relative.as_ref(), size),
            )?;
        }
        return Ok(());
    }
    let chunk_count = paths.len().div_ceil(CHUNK_SIZE);
    let worker_count = jobs.min(chunk_count);
    let (result_sender, result_receiver) =
        std::sync::mpsc::sync_channel::<SampleChunk>(worker_count);
    let mut task_senders = Vec::with_capacity(worker_count);
    let mut task_receivers = Vec::with_capacity(worker_count);
    for _ in 0..worker_count {
        let (sender, receiver) = std::sync::mpsc::sync_channel::<usize>(1);
        task_senders.push(sender);
        task_receivers.push(receiver);
    }
    std::thread::scope(move |scope| {
        let mut handles = Vec::with_capacity(worker_count);
        for receiver in task_receivers {
            let sender = result_sender.clone();
            handles.push(scope.spawn(move || {
                while let Ok(chunk_index) = receiver.recv() {
                    let start = chunk_index * CHUNK_SIZE;
                    let end = (start + CHUNK_SIZE).min(paths.len());
                    let results = paths[start..end]
                        .iter()
                        .map(|relative| sample_signature_for_path(root, relative.as_ref(), size))
                        .collect();
                    if sender.send((chunk_index, results)).is_err() {
                        break;
                    }
                }
            }));
        }
        drop(result_sender);
        for (worker_index, sender) in task_senders.iter().enumerate() {
            sender
                .send(worker_index)
                .map_err(|_| "duplicate sampling worker stopped unexpectedly".to_owned())?;
        }
        let mut next_chunk = worker_count;
        let mut next_to_visit = 0;
        let mut pending: Vec<Option<SampleChunk>> = (0..worker_count).map(|_| None).collect();
        while next_to_visit < chunk_count {
            let (chunk_index, results) = result_receiver
                .recv()
                .map_err(|_| "duplicate sampling workers stopped unexpectedly".to_owned())?;
            let slot = chunk_index % worker_count;
            debug_assert!(pending[slot].is_none());
            pending[slot] = Some((chunk_index, results));
            loop {
                let next_slot = next_to_visit % worker_count;
                if !pending[next_slot]
                    .as_ref()
                    .is_some_and(|(chunk_index, _)| *chunk_index == next_to_visit)
                {
                    break;
                }
                let (_, chunk_results) = pending[next_slot]
                    .take()
                    .expect("ready duplicate sample chunk is present");
                let first_index = next_to_visit * CHUNK_SIZE;
                for (offset, result) in chunk_results.into_iter().enumerate() {
                    visit(first_index + offset, result)?;
                }
                let worker_index = next_to_visit % worker_count;
                next_to_visit += 1;
                if next_chunk < chunk_count {
                    task_senders[worker_index]
                        .send(next_chunk)
                        .map_err(|_| "duplicate sampling worker stopped unexpectedly".to_owned())?;
                    next_chunk += 1;
                }
            }
        }
        drop(task_senders);
        for handle in handles {
            handle.join().expect("duplicate sampling worker panicked");
        }
        Ok(())
    })
}

struct VerifiedDuplicateGroup {
    representative: usize,
    files: Option<DuplicateFileIndices>,
    count: usize,
}

enum DuplicateFileIndices {
    Compact(Vec<u32>),
    Wide(Vec<usize>),
}

impl DuplicateFileIndices {
    fn with_first(index: usize, compact: bool) -> Self {
        if compact {
            Self::Compact(vec![
                u32::try_from(index).expect("compact duplicate path index"),
            ])
        } else {
            Self::Wide(vec![index])
        }
    }

    fn push(&mut self, index: usize) {
        match self {
            Self::Compact(indices) => {
                indices.push(u32::try_from(index).expect("compact duplicate path index"));
            }
            Self::Wide(indices) => indices.push(index),
        }
    }

    fn into_paths(self, paths: &[Cow<'_, str>]) -> Vec<String> {
        match self {
            Self::Compact(indices) => indices
                .into_iter()
                .map(|index| paths[index as usize].to_string())
                .collect(),
            Self::Wide(indices) => indices
                .into_iter()
                .map(|index| paths[index].to_string())
                .collect(),
        }
    }
}

struct SampleSignatureBucket {
    first: VerifiedDuplicateGroup,
    collisions: Option<Vec<VerifiedDuplicateGroup>>,
}

impl SampleSignatureBucket {
    fn groups_mut(&mut self) -> impl Iterator<Item = &mut VerifiedDuplicateGroup> {
        std::iter::once(&mut self.first).chain(self.collisions.iter_mut().flatten())
    }

    fn push_collision(&mut self, group: VerifiedDuplicateGroup) {
        if let Some(collisions) = &mut self.collisions {
            collisions.push(group);
        } else {
            self.collisions = Some(vec![group]);
        }
    }

    fn into_groups(self) -> impl Iterator<Item = VerifiedDuplicateGroup> {
        std::iter::once(self.first).chain(self.collisions.unwrap_or_default())
    }
}

struct DuplicateSizeBucket<'a> {
    first: Option<Cow<'a, str>>,
    duplicates: Option<Vec<Cow<'a, str>>>,
}

impl<'a> DuplicateSizeBucket<'a> {
    fn push(&mut self, path: Cow<'a, str>) {
        if let Some(paths) = &mut self.duplicates {
            paths.push(path);
        } else {
            let first = self
                .first
                .take()
                .expect("single-file size bucket has its first path");
            self.duplicates = Some(vec![first, path]);
        }
    }
}

struct DuplicateOutputGroup {
    size: u64,
    files: Vec<String>,
}

fn compare_duplicate_group_stats(
    left_size: u64,
    left_count: usize,
    left_first_path: &str,
    right_size: u64,
    right_count: usize,
    right_first_path: &str,
) -> std::cmp::Ordering {
    let left_reclaimable = u128::from(left_size) * left_count.saturating_sub(1) as u128;
    let right_reclaimable = u128::from(right_size) * right_count.saturating_sub(1) as u128;
    right_reclaimable
        .cmp(&left_reclaimable)
        .then_with(|| right_size.cmp(&left_size))
        .then_with(|| left_first_path.cmp(right_first_path))
}

fn compare_duplicate_output_groups(
    left: &DuplicateOutputGroup,
    right: &DuplicateOutputGroup,
) -> std::cmp::Ordering {
    compare_duplicate_group_stats(
        left.size,
        left.files.len(),
        left.files.first().map_or("", String::as_str),
        right.size,
        right.files.len(),
        right.files.first().map_or("", String::as_str),
    )
}

impl PartialEq for DuplicateOutputGroup {
    fn eq(&self, other: &Self) -> bool {
        compare_duplicate_output_groups(self, other).is_eq()
    }
}

impl Eq for DuplicateOutputGroup {}

impl PartialOrd for DuplicateOutputGroup {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for DuplicateOutputGroup {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        compare_duplicate_output_groups(self, other)
    }
}

fn run_dupes(args: &[String]) -> Result<(), String> {
    let mut json = false;
    let mut compact = false;
    let mut summary_only = false;
    let mut include_hidden = false;
    let mut exclude_path_contains = Vec::<String>::new();
    let mut min_size = 0;
    let mut max_size = None;
    let mut kind_filter = None;
    let mut extension_filter = None;
    let mut top_groups = None;
    let mut jobs = std::thread::available_parallelism()
        .map_or(1, usize::from)
        .min(4);
    let mut path = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--json" => json = true,
            "--compact" => compact = true,
            "--summary" => summary_only = true,
            "--include-hidden" => include_hidden = true,
            "--exclude-path-contains" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--exclude-path-contains requires a substring".to_owned())?;
                if value.is_empty() {
                    return Err("--exclude-path-contains cannot be empty".to_owned());
                }
                exclude_path_contains.push(value.clone());
            }
            "--min-size" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--min-size requires a value".to_owned())?;
                min_size = parse_size(value)?;
            }
            "--max-size" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--max-size requires a value".to_owned())?;
                max_size = Some(parse_size(value)?);
            }
            "--type" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--type requires a value".to_owned())?;
                kind_filter = Some(parse_asset_kind(value).ok_or_else(|| {
                    format!(
                        "unknown asset type '{value}'; choose video, audio, image, model, texture, project, or other"
                    )
                })?);
            }
            "--extension" => {
                let value = iter.next().ok_or_else(|| {
                    "--extension requires a comma-separated extension list".to_owned()
                })?;
                extension_filter = Some(parse_extensions(value)?);
            }
            "--top-groups" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--top-groups requires a group count".to_owned())?;
                top_groups = Some(value.parse::<usize>().map_err(|_| {
                    format!("invalid --top-groups count '{value}'; use a non-negative integer")
                })?);
            }
            "--jobs" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--jobs requires a positive integer".to_owned())?;
                jobs = value
                    .parse::<usize>()
                    .map_err(|_| format!("invalid --jobs value '{value}'"))?;
                if !(1..=64).contains(&jobs) {
                    return Err("--jobs must be between 1 and 64".to_owned());
                }
            }
            "--help" | "-h" => {
                println!(
                    "Usage: mediaforge dupes [PATH] [--json] [--compact] [--summary] [--include-hidden] [--exclude-path-contains TEXT ...] [--type TYPE] [--min-size SIZE] [--max-size SIZE] [--extension EXTENSIONS] [--top-groups N] [--jobs N]\n\nFind byte-for-byte duplicate files. Files are compared in fixed-size chunks to keep memory use low. --summary verifies every candidate but omits duplicate paths from output. --compact removes formatting whitespace from JSON output and requires --json. Repeat --exclude-path-contains to skip matching paths case-insensitively; matching directories are pruned before traversal. --jobs controls concurrent sample reads (1–64; default up to 4). --min-size and --max-size limit which file sizes are checked; both accept bytes or K/M/G/T units. --top-groups limits displayed groups, ordered by reclaimable space; summary totals include all groups. --type accepts video, audio, image, model, texture, project, or other. --extension accepts a comma-separated list such as mov,mxf; use none to select files without an extension. Fractional size thresholds round up to the next byte."
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
    if compact && !json {
        return Err("--compact can only be used with --json".to_owned());
    }
    let path = path.unwrap_or_else(|| PathBuf::from("."));
    if max_size.is_some_and(|max_size| max_size < min_size) {
        return Err("--max-size must be greater than or equal to --min-size".to_owned());
    }
    let root = fs::canonicalize(&path)
        .map_err(|error| format!("cannot access {}: {error}", path.display()))?;
    if !root.is_dir() {
        return Err(format!("expected a directory: {}", root.display()));
    }
    let ignored = ignored_by_git(&root)?;
    let mut records = Vec::new();
    let kinds = kind_filter.map(|kind| [kind]);
    let exclude_path_contains_lower = exclude_path_contains
        .iter()
        .map(|value| value.to_lowercase())
        .collect::<Vec<_>>();
    let filters = StrictWalkFilters {
        kinds: kinds.as_ref().map(|values| values.as_slice()),
        extensions: extension_filter.as_ref(),
        sort_entries_per_directory: true,
        min_size: Some(min_size),
        max_size,
        excluded_subtree: None,
        exclude_path_contains_lower: Some(&exclude_path_contains_lower),
        omit_absolute_path: true,
        store_relative_path: false,
        omit_modified_time: true,
        omit_extension: true,
    };
    let mut duplicate_sizes = if summary_only {
        let mut by_size = HashMap::<u64, DuplicateSizeBucket<'static>>::new();
        walk_strict_filtered_with_callback(
            &root,
            &root,
            include_hidden,
            SKIP_DIRS,
            &ignored,
            &filters,
            |record| {
                match by_size.entry(record.size) {
                    std::collections::hash_map::Entry::Occupied(mut entry) => {
                        entry.get_mut().push(Cow::Owned(record.relative));
                    }
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        entry.insert(DuplicateSizeBucket {
                            first: Some(Cow::Owned(record.relative)),
                            duplicates: None,
                        });
                    }
                }
                Ok(())
            },
        )?;
        by_size
            .into_iter()
            .filter_map(|(size, bucket)| bucket.duplicates.map(|paths| (size, paths)))
            .collect::<Vec<_>>()
    } else {
        walk_strict_filtered(
            &root,
            &root,
            include_hidden,
            SKIP_DIRS,
            &ignored,
            &filters,
            &mut records,
        )?;
        let mut by_size = HashMap::<u64, DuplicateSizeBucket<'_>>::new();
        for record in &records {
            match by_size.entry(record.size) {
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    entry
                        .get_mut()
                        .push(Cow::Borrowed(record.relative.as_str()));
                }
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(DuplicateSizeBucket {
                        first: Some(Cow::Borrowed(record.relative.as_str())),
                        duplicates: None,
                    });
                }
            }
        }
        by_size
            .into_iter()
            .filter_map(|(size, bucket)| bucket.duplicates.map(|paths| (size, paths)))
            .collect::<Vec<_>>()
    };
    duplicate_sizes.sort_unstable_by_key(|(size, _)| *size);

    let retain_group_paths = !summary_only && top_groups != Some(0);
    let mut groups: Vec<(u64, Vec<String>)> = Vec::new();
    let mut top_group_heap = BinaryHeap::<DuplicateOutputGroup>::new();
    let mut total_groups = 0_usize;
    let mut duplicate_files = 0_usize;
    let mut reclaimable = 0_u128;
    let mut comparison_buffers = (Vec::new(), Vec::new());
    for (size, paths) in duplicate_sizes {
        let compact_indices = u32::try_from(paths.len()).is_ok();
        let mut by_sample: HashMap<u64, SampleSignatureBucket> = HashMap::new();
        for_each_sample_signature(&root, &paths, size, jobs, |index, signature| {
            let relative = paths[index].as_ref();
            let signature = signature?;
            let candidate = root.join(relative);
            let entry = by_sample.entry(signature);
            let bucket = match entry {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(SampleSignatureBucket {
                        first: VerifiedDuplicateGroup {
                            representative: index,
                            files: None,
                            count: 1,
                        },
                        collisions: None,
                    });
                    return Ok(());
                }
                std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            };
            let mut matched = false;
            for group in bucket.groups_mut() {
                let representative = root.join(paths[group.representative].as_ref());
                match files_identical(
                    &representative,
                    &candidate,
                    size,
                    &mut comparison_buffers.0,
                    &mut comparison_buffers.1,
                ) {
                    Ok(true) => {
                        group.count += 1;
                        if retain_group_paths {
                            let representative = group.representative;
                            let files = group.files.get_or_insert_with(|| {
                                DuplicateFileIndices::with_first(representative, compact_indices)
                            });
                            files.push(index);
                        }
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
                bucket.push_collision(VerifiedDuplicateGroup {
                    representative: index,
                    files: None,
                    count: 1,
                });
            }
            Ok(())
        })?;
        for group in by_sample
            .into_values()
            .flat_map(SampleSignatureBucket::into_groups)
            .filter(|group| group.count > 1)
        {
            total_groups += 1;
            duplicate_files += group.count;
            reclaimable += u128::from(size) * group.count.saturating_sub(1) as u128;
            if let Some(file_indices) = group.files {
                if let Some(limit) = top_groups {
                    let first_path = paths[group.representative].as_ref();
                    let is_top_group = top_group_heap.len() < limit
                        || top_group_heap.peek().is_some_and(|worst| {
                            compare_duplicate_group_stats(
                                size,
                                group.count,
                                first_path,
                                worst.size,
                                worst.files.len(),
                                worst.files.first().map_or("", String::as_str),
                            )
                            .is_lt()
                        });
                    if is_top_group {
                        let files = file_indices.into_paths(&paths);
                        let candidate = DuplicateOutputGroup { size, files };
                        if top_group_heap.len() == limit {
                            top_group_heap.pop();
                        }
                        top_group_heap.push(candidate);
                    }
                } else {
                    let files = file_indices.into_paths(&paths);
                    groups.push((size, files));
                }
            }
        }
    }
    groups.extend(
        top_group_heap
            .into_iter()
            .map(|group| (group.size, group.files)),
    );
    if !summary_only {
        groups.sort_unstable_by(|left, right| {
            (u128::from(right.0) * right.1.len().saturating_sub(1) as u128)
                .cmp(&(u128::from(left.0) * left.1.len().saturating_sub(1) as u128))
                .then_with(|| right.0.cmp(&left.0))
                .then_with(|| left.1.first().cmp(&right.1.first()))
        });
    }
    let displayed_groups = if summary_only {
        0
    } else {
        top_groups.unwrap_or(total_groups).min(total_groups)
    };
    let truncated_groups = total_groups - displayed_groups;
    if json {
        let stdout = io::stdout();
        let output = io::BufWriter::with_capacity(64 * 1024, stdout.lock());
        let mut output = JsonWhitespaceWriter::new(output, compact);
        write!(
            output,
            "{{\n  \"root\": {},\n  \"min_size_bytes\": {},\n  \"max_size_bytes\": {},\n  \"exclude_path_contains\": [{}],\n  \"groups_total\": {},\n  \"groups_returned\": {},\n  \"groups_truncated\": {},\n  \"groups\": [",
            json_string(&root.to_string_lossy()),
            min_size,
            max_size.map_or_else(|| "null".to_owned(), |size| size.to_string()),
            exclude_path_contains
                .iter()
                .map(|value| json_string(value))
                .collect::<Vec<_>>()
                .join(","),
            total_groups,
            displayed_groups,
            truncated_groups
        )
        .map_err(|error| format!("could not write duplicate JSON: {error}"))?;
        for (index, (size, files)) in groups.iter().take(displayed_groups).enumerate() {
            let comma = if index + 1 == displayed_groups {
                ""
            } else {
                ","
            };
            write!(output, "    {{\"size_bytes\": {}, \"files\": [", size)
                .map_err(|error| format!("could not write duplicate JSON: {error}"))?;
            for (file_index, file) in files.iter().enumerate() {
                if file_index > 0 {
                    output
                        .write_all(b", ")
                        .map_err(|error| format!("could not write duplicate JSON: {error}"))?;
                }
                write_json_string(&mut output, file)
                    .map_err(|error| format!("could not write duplicate JSON: {error}"))?;
            }
            writeln!(output, "]}}{}", comma)
                .map_err(|error| format!("could not write duplicate JSON: {error}"))?;
        }
        writeln!(
            output,
            "  ],\n  \"duplicate_files\": {},\n  \"reclaimable_bytes\": {},\n  \"reclaimable_size\": {}\n}}",
            duplicate_files,
            reclaimable,
            json_string(&human_size_wide(reclaimable))
        )
        .map_err(|error| format!("could not write duplicate JSON: {error}"))?;
        output
            .flush()
            .map_err(|error| format!("could not write duplicate JSON: {error}"))?;
    } else {
        println!(
            "Verified duplicate files: {duplicate_files} · potential reclaim: {}",
            human_size_wide(reclaimable)
        );
        if groups.is_empty() {
            println!("No byte-for-byte duplicate groups found.");
        }
        for (size, files) in groups.iter().take(displayed_groups) {
            println!("\n{} each", human_size(*size));
            for file in files {
                println!("  {file}");
            }
        }
        if truncated_groups > 0 {
            println!(
                "\nOmitted {truncated_groups} smaller reclaimable group(s); summary totals include all verified groups."
            );
        }
    }
    Ok(())
}

fn run_scan(args: &[String]) -> Result<(), String> {
    let resolved_args = scan_args_with_settings(args)?;
    let args = resolved_args.as_slice();
    let mut json = false;
    let mut compact = false;
    let mut csv = false;
    let mut show_filters = false;
    let mut include_same_size = true;
    let mut include_hidden = false;
    let mut strict = false;
    let mut path = None;
    let mut kind_filter = None;
    let mut min_size = None;
    let mut max_size = None;
    let mut modified_since = None;
    let mut modified_until = None;
    let mut modified_within = None;
    let mut path_contains = Vec::new();
    let mut exclude_path_contains = Vec::new();
    let mut extension_filter = None;
    let mut largest_limit = None;
    let mut csv_sort: Option<ScanCsvSort> = None;
    let mut csv_info = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--json" => json = true,
            "--compact" => compact = true,
            "--csv" => csv = true,
            "--show-filters" => show_filters = true,
            "--no-same-size" => include_same_size = false,
            "--csv-info" => csv_info = true,
            "--include-hidden" => include_hidden = true,
            "--strict" => strict = true,
            "--type" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--type requires a value".to_owned())?;
                kind_filter = Some(parse_asset_kind(value).ok_or_else(|| {
                    format!(
                        "unknown asset type '{value}'; choose video, audio, image, model, texture, project, or other"
                    )
                })?);
            }
            "--min-size" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--min-size requires a value".to_owned())?;
                min_size = Some(parse_size(value)?);
            }
            "--max-size" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--max-size requires a value".to_owned())?;
                max_size = Some(parse_size(value)?);
            }
            "--modified-since" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--modified-since requires Unix seconds".to_owned())?;
                modified_since = Some(value.parse::<u64>().map_err(|_| {
                    format!(
                        "invalid --modified-since value '{value}'; use non-negative Unix seconds"
                    )
                })?);
            }
            "--modified-until" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--modified-until requires Unix seconds".to_owned())?;
                modified_until = Some(value.parse::<u64>().map_err(|_| {
                    format!(
                        "invalid --modified-until value '{value}'; use non-negative Unix seconds"
                    )
                })?);
            }
            "--modified-within" => {
                let value = iter.next().ok_or_else(|| {
                    "--modified-within requires a duration such as 7d or 12h".to_owned()
                })?;
                modified_within = Some(parse_duration_seconds(value).map_err(|error| {
                    format!("invalid --modified-within value '{value}': {error}")
                })?);
            }
            "--path-contains" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--path-contains requires text".to_owned())?;
                if value.is_empty() {
                    return Err("--path-contains cannot be empty".to_owned());
                }
                path_contains.push(value.clone());
            }
            "--exclude-path-contains" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--exclude-path-contains requires text".to_owned())?;
                if value.is_empty() {
                    return Err("--exclude-path-contains cannot be empty".to_owned());
                }
                exclude_path_contains.push(value.clone());
            }
            "--largest" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--largest requires a file count".to_owned())?;
                largest_limit = Some(value.parse::<usize>().map_err(|_| {
                    format!("invalid --largest count '{value}'; use a non-negative integer")
                })?);
            }
            "--extension" => {
                let value = iter.next().ok_or_else(|| {
                    "--extension requires a comma-separated extension list".to_owned()
                })?;
                extension_filter
                    .get_or_insert_with(HashSet::new)
                    .extend(parse_extensions(value)?);
            }
            "--sort" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--sort requires path, size, or modified".to_owned())?;
                csv_sort = Some(match value.to_ascii_lowercase().as_str() {
                    "path" => ScanCsvSort::Path,
                    "size" => ScanCsvSort::Size,
                    "modified" => ScanCsvSort::Modified,
                    _ => {
                        return Err(format!(
                            "unknown scan sort '{value}'; choose path, size, or modified"
                        ));
                    }
                });
            }
            "--help" | "-h" => {
                println!(
                    "Usage: mediaforge scan [PATH] [--json|--csv] [--compact] [--csv-info] [--show-filters] [--include-hidden] [--strict] [--no-same-size] [--type TYPE] [--min-size SIZE] [--max-size SIZE] [--modified-since UNIX_SECONDS] [--modified-until UNIX_SECONDS] [--modified-within DURATION] [--path-contains TEXT ...] [--exclude-path-contains TEXT ...] [--extension EXTENSIONS ...] [--largest N] [--sort path|size|modified] [--settings FILE]\n\nSummarize media and game assets in a folder. --compact removes formatting whitespace from JSON output and requires --json. --show-filters prints the resolved filters as JSON and exits without scanning the target path, so it can preview --settings plus command-line overrides. The preview reports whether --largest applies and whether its value is explicit or the output-format default; CSV always includes all matching rows. --no-same-size omits the potentially large same-size candidate path list from JSON while retaining the group count. --min-size and --max-size select an inclusive byte range; fractional size values are rounded up to whole bytes. --modified-since and --modified-until select an inclusive modification-time range in Unix seconds. --modified-within selects recent files using a duration such as 12h or 7d and cannot be combined with those absolute-time filters. --settings loads key=value scan filters; explicit command-line values are applied afterward. --path-contains selects relative paths by a case-insensitive substring; repeat it to require every substring. --exclude-path-contains removes paths matching any repeated case-insensitive substring. --extension accepts comma-separated extensions; repeat it to combine more extensions. Permission and file-system errors are skipped by default; text/JSON report incomplete scans and CSV writes warnings to stderr. JSON uses schema_version 1 and includes incomplete, skipped_paths, up to 100 skipped_path_examples, skipped_paths_truncated, and the applied filters. Use --strict to fail on the first traversal error instead. --csv emits one row per matched file with path, type, extension, byte size, and modification time in Unix seconds; it always includes all matches. Add --csv-info to write a JSON scan-info record to stderr without changing CSV stdout. Use --sort path, --sort size, or --sort modified to control CSV row order; size order is largest first and modified order is newest first, with unavailable times last and deterministic path tie-breaking. --largest controls how many biggest files are shown (default: 10 in text, 20 in JSON). Defaults to the current directory.\n\nTypes: video, audio, image, model, texture, project, other\nExtensions: comma-separated, case-insensitive (for example exr,tif); use none to select files without an extension\nSizes: bytes, KiB/MiB/GiB, or K/M/G (for example 500M); fractional values round up to the next byte"
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
    if json && csv {
        return Err("--json and --csv cannot be used together".to_owned());
    }
    if compact && !json {
        return Err("--compact can only be used with --json".to_owned());
    }
    if csv_sort.is_some() && !csv {
        return Err("--sort can only be used with --csv".to_owned());
    }
    if csv_info && !csv {
        return Err("--csv-info can only be used with --csv".to_owned());
    }
    if !include_same_size && !json {
        return Err("--no-same-size can only be used with --json".to_owned());
    }
    if max_size.is_some_and(|max| min_size.is_some_and(|min| max < min)) {
        return Err("--max-size must be greater than or equal to --min-size".to_owned());
    }
    if modified_within.is_some() && (modified_since.is_some() || modified_until.is_some()) {
        return Err(
            "--modified-within cannot be combined with --modified-since or --modified-until"
                .to_owned(),
        );
    }
    if modified_until.is_some_and(|until| modified_since.is_some_and(|since| until < since)) {
        return Err(
            "--modified-until must be greater than or equal to --modified-since".to_owned(),
        );
    }
    if let Some(within) = modified_within {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| "system clock is before the Unix epoch".to_owned())?
            .as_secs();
        modified_since = Some(now.saturating_sub(within));
    }
    let path = path.unwrap_or_else(|| PathBuf::from("."));
    let largest_limit_default = if json { 20 } else { 10 };
    let largest_limit_is_explicit = largest_limit.is_some();
    let largest_limit = largest_limit.unwrap_or(largest_limit_default);
    if show_filters {
        print_scan_filter_preview(ScanFilterPreviewOptions {
            path: &path,
            json,
            compact,
            csv,
            csv_sort,
            largest_limit,
            largest_limit_default,
            largest_limit_is_explicit,
            filters: ScanFilterOptions {
                kind: kind_filter,
                min_size,
                max_size,
                modified_since,
                modified_until,
                modified_within,
                path_contains: &path_contains,
                exclude_path_contains: &exclude_path_contains,
                extensions: extension_filter.as_ref(),
                include_hidden,
                strict,
            },
        })?;
        return Ok(());
    }
    let root =
        fs::canonicalize(&path).map_err(|e| format!("cannot access {}: {e}", path.display()))?;
    if !root.is_dir() {
        return Err(format!("expected a directory: {}", root.display()));
    }
    let ignored = ignored_by_git(&root)?;
    let path_contains_lower = path_contains
        .iter()
        .map(|value| value.to_lowercase())
        .collect::<Vec<_>>();
    let exclude_path_contains_lower = exclude_path_contains
        .iter()
        .map(|value| value.to_lowercase())
        .collect::<Vec<_>>();
    let traversal_filters = ScanTraversalFilters {
        kind: kind_filter,
        min_size,
        max_size,
        modified_since,
        modified_until,
        include_modified: csv,
        extensions: extension_filter.as_ref(),
        path_contains_lower: &path_contains_lower,
        exclude_path_contains_lower: &exclude_path_contains_lower,
        sort_entries_per_directory: csv_sort.is_none() && (csv || strict),
    };
    let stream_csv = csv && csv_sort.is_none() && !strict;
    let stdout = io::stdout();
    let mut streaming_csv =
        stream_csv.then(|| io::BufWriter::with_capacity(64 * 1024, stdout.lock()));
    let mut json_summary =
        (json && !include_same_size).then(|| ScanJsonAccumulator::new(largest_limit));
    if let Some(output) = streaming_csv.as_mut() {
        write_csv_header(output).map_err(|error| format!("could not write CSV output: {error}"))?;
    }
    let record_consumer = if let Some(output) = streaming_csv.as_mut() {
        Some(ScanRecordConsumer::Csv(output as &mut dyn Write))
    } else {
        json_summary.as_mut().map(ScanRecordConsumer::Json)
    };
    let ScanWalkResult {
        mut records,
        file_count,
        skipped_paths,
        mut skipped_examples,
    } = walk_with_skip_dirs_mode_counted(ScanWalkConfig {
        root: &root,
        include_hidden,
        skip_dirs: SKIP_DIRS,
        ignored: &ignored,
        filters: &traversal_filters,
        strict,
        record_consumer,
    })?;
    skipped_examples.sort_unstable_by(|left, right| compare_paths_case_insensitive(left, right));
    if let Some(sort) = csv_sort {
        match sort {
            ScanCsvSort::Size => records.sort_unstable_by(|left, right| {
                std::cmp::Reverse(left.size)
                    .cmp(&std::cmp::Reverse(right.size))
                    .then_with(|| compare_paths_case_insensitive(&left.relative, &right.relative))
            }),
            ScanCsvSort::Path => records.sort_unstable_by(|a, b| a.relative.cmp(&b.relative)),
            ScanCsvSort::Modified => records.sort_unstable_by(|left, right| {
                std::cmp::Reverse(
                    left.modified
                        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok()),
                )
                .cmp(&std::cmp::Reverse(right.modified.and_then(|time| {
                    time.duration_since(std::time::UNIX_EPOCH).ok()
                })))
                .then_with(|| compare_paths_case_insensitive(&left.relative, &right.relative))
            }),
        }
    }
    if csv {
        if csv_info {
            print_scan_csv_info(
                &root,
                file_count,
                skipped_paths,
                &skipped_examples,
                ScanFilterOptions {
                    kind: kind_filter,
                    min_size,
                    max_size,
                    modified_since,
                    modified_until,
                    modified_within,
                    path_contains: &path_contains,
                    exclude_path_contains: &exclude_path_contains,
                    extensions: extension_filter.as_ref(),
                    include_hidden,
                    strict,
                },
                csv_sort,
            );
        } else if skipped_paths > 0 {
            eprintln!("Warning: incomplete scan; skipped {skipped_paths} unreadable path(s).");
            for path in &skipped_examples {
                eprintln!("  skipped: {path}");
            }
            if skipped_paths > skipped_examples.len() {
                eprintln!("  ... and {} more", skipped_paths - skipped_examples.len());
            }
        }
        if let Some(output) = streaming_csv.as_mut() {
            output
                .flush()
                .map_err(|error| format!("could not write CSV output: {error}"))?;
        } else {
            print_csv(&records)?;
        }
    } else if json {
        let filters = ScanFilterOptions {
            kind: kind_filter,
            min_size,
            max_size,
            modified_since,
            modified_until,
            modified_within,
            path_contains: &path_contains,
            exclude_path_contains: &exclude_path_contains,
            extensions: extension_filter.as_ref(),
            include_hidden,
            strict,
        };
        if let Some(summary) = json_summary.as_mut() {
            print_json_summary(
                &root,
                summary,
                skipped_paths,
                &skipped_examples,
                filters,
                compact,
            )?;
        } else {
            print_json(
                &root,
                &records,
                ScanJsonOutputOptions {
                    largest_limit,
                    include_same_size,
                    compact,
                },
                skipped_paths,
                &skipped_examples,
                filters,
            )?;
        }
    } else {
        print_report(
            &root,
            &records,
            largest_limit,
            skipped_paths,
            &skipped_examples,
        );
    }
    Ok(())
}

const SNAPSHOT_MAGIC_V1: &[u8; 8] = b"MFSP\x01\0\0\0";
const SNAPSHOT_MAGIC: &[u8; 8] = b"MFSP\x02\0\0\0";
const SNAPSHOT_SKIP_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "node_modules",
    "__pycache__",
    ".venv",
    "venv",
    "Library",
    "Temp",
    "Obj",
    "Intermediate",
    "DerivedDataCache",
    "Binaries",
    ".godot",
    ".import",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
struct SnapshotEntry {
    size: u64,
    has_modified_ns: bool,
    modified_ns: i128,
    content_hash: [u8; 32],
}

type SnapshotEntries = Vec<(String, SnapshotEntry)>;

const _: () = assert!(std::mem::size_of::<SnapshotEntry>() <= 64);

impl SnapshotEntry {
    fn modified_ns(self) -> Option<i128> {
        self.has_modified_ns.then_some(self.modified_ns)
    }
}

#[derive(Default)]
struct SnapshotTypeStats {
    added: usize,
    removed: usize,
    modified: usize,
    unchanged: usize,
    added_bytes: u128,
    removed_bytes: u128,
    modified_size_delta_bytes: i128,
}

#[derive(Default)]
struct SnapshotSizeStats {
    added: usize,
    removed: usize,
    modified: usize,
    added_bytes: u128,
    removed_bytes: u128,
    modified_size_delta_bytes: i128,
}

fn snapshot_entry(record: &SnapshotFileRecord, content_hash: [u8; 32]) -> SnapshotEntry {
    let modified_ns =
        record.modified.map(
            |modified| match modified.duration_since(std::time::UNIX_EPOCH) {
                Ok(duration) => i128::try_from(duration.as_nanos()).unwrap_or(i128::MAX),
                Err(error) => -i128::try_from(error.duration().as_nanos()).unwrap_or(i128::MAX),
            },
        );
    SnapshotEntry {
        size: record.size,
        has_modified_ns: modified_ns.is_some(),
        modified_ns: modified_ns.unwrap_or_default(),
        content_hash,
    }
}

struct Sha256 {
    state: [u32; 8],
    buffer: [u8; 64],
    buffered: usize,
    length: u64,
}

impl Sha256 {
    fn new() -> Self {
        Self {
            state: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            buffer: [0; 64],
            buffered: 0,
            length: 0,
        }
    }

    fn update(&mut self, mut input: &[u8]) {
        self.length = self.length.wrapping_add(input.len() as u64);
        if self.buffered != 0 {
            let take = (64 - self.buffered).min(input.len());
            self.buffer[self.buffered..self.buffered + take].copy_from_slice(&input[..take]);
            self.buffered += take;
            input = &input[take..];
            if self.buffered == 64 {
                let block = self.buffer;
                self.compress(&block);
                self.buffered = 0;
            }
        }
        while input.len() >= 64 {
            let block: &[u8; 64] = input[..64].try_into().expect("fixed-size block");
            self.compress(block);
            input = &input[64..];
        }
        self.buffer[..input.len()].copy_from_slice(input);
        self.buffered = input.len();
    }

    fn compress(&mut self, block: &[u8; 64]) {
        const K: [u32; 64] = [
            0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
            0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
            0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
            0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
            0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
            0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
            0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
            0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
            0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
            0xc67178f2,
        ];
        let mut words = [0_u32; 64];
        for (index, chunk) in block.as_chunks::<4>().0.iter().enumerate() {
            words[index] = u32::from_be_bytes(*chunk);
        }
        for index in 16..64 {
            let x = words[index - 15];
            let y = words[index - 2];
            let s0 = x.rotate_right(7) ^ x.rotate_right(18) ^ (x >> 3);
            let s1 = y.rotate_right(17) ^ y.rotate_right(19) ^ (y >> 10);
            words[index] = words[index - 16]
                .wrapping_add(s0)
                .wrapping_add(words[index - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for index in 0..64 {
            let sum1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choose = (e & f) ^ (!e & g);
            let t1 = h
                .wrapping_add(sum1)
                .wrapping_add(choose)
                .wrapping_add(K[index])
                .wrapping_add(words[index]);
            let sum0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let t2 = sum0.wrapping_add(majority);
            [a, b, c, d, e, f, g, h] = [t1.wrapping_add(t2), a, b, c, d.wrapping_add(t1), e, f, g];
        }
        for (slot, value) in self.state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(value);
        }
    }

    fn finish(mut self) -> [u8; 32] {
        let bit_length = self.length.wrapping_mul(8);
        self.buffer[self.buffered] = 0x80;
        self.buffered += 1;
        if self.buffered > 56 {
            self.buffer[self.buffered..].fill(0);
            let block = self.buffer;
            self.compress(&block);
            self.buffer = [0; 64];
        } else {
            self.buffer[self.buffered..56].fill(0);
        }
        self.buffer[56..].copy_from_slice(&bit_length.to_be_bytes());
        let block = self.buffer;
        self.compress(&block);
        let mut digest = [0; 32];
        for (chunk, word) in digest.as_chunks_mut::<4>().0.iter_mut().zip(self.state) {
            *chunk = word.to_be_bytes();
        }
        digest
    }
}

const SNAPSHOT_HASH_BUFFER_SIZE: usize = 256 * 1024;

fn hash_snapshot_file(
    root: &Path,
    record: &SnapshotFileRecord,
    buffer: &mut [u8; SNAPSHOT_HASH_BUFFER_SIZE],
) -> Result<[u8; 32], String> {
    let path = root.join(&record.path);
    let mut file = File::open(&path)
        .map_err(|error| format!("cannot read {} for hashing: {error}", path.display()))?;
    let before = file
        .metadata()
        .map_err(|error| format!("cannot inspect {} before hashing: {error}", path.display()))?;
    if before.len() != record.size || before.modified().ok() != record.modified {
        return Err(format!(
            "file changed during snapshot scan: {}",
            path.display()
        ));
    }
    let mut hasher = Sha256::new();
    let mut remaining = record.size;
    while remaining > 0 {
        let requested = remaining.min(buffer.len() as u64) as usize;
        let count = read_up_to_full_or_eof(&mut file, &mut buffer[..requested])
            .map_err(|error| format!("could not hash {}: {error}", path.display()))?;
        if count != requested {
            return Err(format!(
                "file shortened while hashing snapshot: {}",
                path.display()
            ));
        }
        hasher.update(&buffer[..count]);
        remaining -= count as u64;
    }
    let mut extra = [0_u8; 1];
    if read_retry_interrupted(&mut file, &mut extra)
        .map_err(|error| format!("could not verify {} after hashing: {error}", path.display()))?
        != 0
    {
        return Err(format!(
            "file grew while hashing snapshot: {}",
            path.display()
        ));
    }
    let after = file
        .metadata()
        .map_err(|error| format!("cannot inspect {} after hashing: {error}", path.display()))?;
    if after.len() != record.size || after.modified().ok() != record.modified {
        return Err(format!(
            "file changed while hashing snapshot: {}",
            path.display()
        ));
    }
    Ok(hasher.finish())
}

fn collect_snapshot_entries(
    root: &Path,
    records: &[SnapshotFileRecord],
    hash_content: bool,
    jobs: usize,
) -> Result<Vec<SnapshotEntry>, String> {
    if jobs == 0 {
        return Err("snapshot worker count must be greater than zero".to_owned());
    }
    if let Some(duplicate) = records
        .windows(2)
        .find(|pair| pair[0].relative == pair[1].relative)
    {
        return Err(format!(
            "multiple files resolve to the same portable path: {}",
            duplicate[0].relative
        ));
    }
    if hash_content && !records.is_empty() {
        let next = std::sync::atomic::AtomicUsize::new(0);
        let next = &next;
        let cancelled = std::sync::atomic::AtomicBool::new(false);
        let cancelled = &cancelled;
        let worker_count = jobs.min(records.len());
        let (sender, receiver) = std::sync::mpsc::sync_channel(worker_count.saturating_mul(2));
        let hash_error = std::thread::scope(|scope| {
            let mut handles = Vec::with_capacity(worker_count);
            for _ in 0..worker_count {
                let sender = sender.clone();
                handles.push(scope.spawn(move || {
                    let mut buffer = [0_u8; SNAPSHOT_HASH_BUFFER_SIZE];
                    loop {
                        if cancelled.load(std::sync::atomic::Ordering::Acquire) {
                            break;
                        }
                        let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(record) = records.get(index) else {
                            break;
                        };
                        let result = hash_snapshot_file(root, record, &mut buffer);
                        if result.is_err() {
                            cancelled.store(true, std::sync::atomic::Ordering::Release);
                        }
                        if sender.send((index, result)).is_err() {
                            break;
                        }
                    }
                }));
            }
            drop(sender);
            let mut entries = records
                .iter()
                .map(|record| snapshot_entry(record, [0; 32]))
                .collect::<Vec<_>>();
            let mut first_error = None;
            for (index, result) in receiver {
                match result {
                    Ok(content_hash) => {
                        entries[index] = snapshot_entry(&records[index], content_hash);
                    }
                    Err(error) if first_error.is_none() => {
                        cancelled.store(true, std::sync::atomic::Ordering::Release);
                        first_error = Some(error);
                    }
                    Err(_) => {}
                }
            }
            for handle in handles {
                handle.join().expect("snapshot hashing worker panicked");
            }
            (first_error, entries)
        });
        let (hash_error, entries) = hash_error;
        if let Some(error) = hash_error {
            return Err(error);
        }
        return Ok(entries);
    }
    Ok(records
        .iter()
        .map(|record| snapshot_entry(record, [0; 32]))
        .collect())
}

fn snapshot_entries_equal(old: SnapshotEntry, new: SnapshotEntry, hash_content: bool) -> bool {
    if hash_content {
        old.content_hash == new.content_hash
    } else {
        old.size == new.size && old.modified_ns() == new.modified_ns()
    }
}

fn snapshot_records_contain_sorted_path(
    records: &[SnapshotFileRecord],
    relative: &str,
    cursor: &mut usize,
) -> bool {
    while *cursor < records.len() && records[*cursor].relative.as_str() < relative {
        *cursor += 1;
    }
    records
        .get(*cursor)
        .is_some_and(|record| record.relative == relative)
}

fn snapshot_entries_find_sorted<'a>(
    entries: &'a SnapshotEntries,
    relative: &str,
    cursor: &mut usize,
) -> Option<&'a SnapshotEntry> {
    while *cursor < entries.len() && entries[*cursor].0.as_str() < relative {
        *cursor += 1;
    }
    entries
        .get(*cursor)
        .filter(|(path, _)| path == relative)
        .map(|(_, entry)| entry)
}

fn snapshot_path_is_valid(path: &str) -> bool {
    !path.is_empty()
        && !path.contains('\0')
        && !path.starts_with('/')
        && !path.contains('\\')
        && !path
            .split('/')
            .next()
            .is_some_and(|component| component.contains(':'))
        && path
            .split('/')
            .all(|component| !component.is_empty() && component != "." && component != "..")
}

fn read_snapshot_manifest(path: &Path) -> Result<(bool, bool, bool, SnapshotEntries), String> {
    const MAX_ENTRIES: u64 = 10_000_000;
    const MAX_PATH_BYTES: usize = 1024 * 1024;
    let file = File::open(path)
        .map_err(|error| format!("cannot open snapshot {}: {error}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut magic = [0; SNAPSHOT_MAGIC.len()];
    reader
        .read_exact(&mut magic)
        .map_err(|error| format!("cannot read snapshot header: {error}"))?;
    if &magic != SNAPSHOT_MAGIC && &magic != SNAPSHOT_MAGIC_V1 {
        return Err("unrecognized or unsupported snapshot format".to_owned());
    }
    let mut flags = [0];
    reader
        .read_exact(&mut flags)
        .map_err(|error| format!("truncated snapshot options: {error}"))?;
    let version = if &magic == SNAPSHOT_MAGIC { 2 } else { 1 };
    if flags[0] & if version == 2 { !0b111 } else { !0b11 } != 0 {
        return Err("snapshot contains unsupported option flags".to_owned());
    }
    let mut count_bytes = [0; 8];
    reader
        .read_exact(&mut count_bytes)
        .map_err(|error| format!("truncated snapshot count: {error}"))?;
    let count = u64::from_le_bytes(count_bytes);
    if count > MAX_ENTRIES {
        return Err(format!("snapshot contains too many entries ({count})"));
    }
    let mut entries = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let mut length_bytes = [0; 4];
        reader
            .read_exact(&mut length_bytes)
            .map_err(|error| format!("truncated snapshot path length: {error}"))?;
        let length = u32::from_le_bytes(length_bytes) as usize;
        if length == 0 || length > MAX_PATH_BYTES {
            return Err("snapshot contains an invalid path length".to_owned());
        }
        let mut path_bytes = vec![0; length];
        reader
            .read_exact(&mut path_bytes)
            .map_err(|error| format!("truncated snapshot path: {error}"))?;
        let relative = String::from_utf8(path_bytes)
            .map_err(|error| format!("snapshot path is not UTF-8: {error}"))?;
        if !snapshot_path_is_valid(&relative) {
            return Err(format!(
                "snapshot contains an unsafe relative path: {relative:?}"
            ));
        }
        let mut size_bytes = [0; 8];
        reader
            .read_exact(&mut size_bytes)
            .map_err(|error| format!("truncated snapshot size: {error}"))?;
        let mut time_flag = [0];
        reader
            .read_exact(&mut time_flag)
            .map_err(|error| format!("truncated snapshot timestamp: {error}"))?;
        let modified_ns = match time_flag[0] {
            0 => None,
            1 => {
                let mut time_bytes = [0; 16];
                reader
                    .read_exact(&mut time_bytes)
                    .map_err(|error| format!("truncated snapshot timestamp: {error}"))?;
                Some(i128::from_le_bytes(time_bytes))
            }
            _ => return Err("snapshot contains an invalid timestamp flag".to_owned()),
        };
        let content_hash = if version == 2 && flags[0] & 4 != 0 {
            let mut hash = [0; 32];
            reader
                .read_exact(&mut hash)
                .map_err(|error| format!("truncated snapshot content hash: {error}"))?;
            Some(hash)
        } else {
            None
        };
        let snapshot_entry = SnapshotEntry {
            size: u64::from_le_bytes(size_bytes),
            has_modified_ns: modified_ns.is_some(),
            modified_ns: modified_ns.unwrap_or_default(),
            content_hash: content_hash.unwrap_or_default(),
        };
        entries.push((relative, snapshot_entry));
    }
    entries.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));
    if let Some(duplicate) = entries.windows(2).find(|pair| pair[0].0 == pair[1].0) {
        return Err(format!(
            "snapshot contains a duplicate path: {}",
            duplicate[0].0
        ));
    }
    let mut trailing = [0];
    if reader
        .read(&mut trailing)
        .map_err(|error| format!("could not finish reading snapshot: {error}"))?
        != 0
    {
        return Err("snapshot contains trailing data".to_owned());
    }
    Ok((
        flags[0] & 1 != 0,
        flags[0] & 2 != 0,
        version == 2 && flags[0] & 4 != 0,
        entries,
    ))
}

fn snapshot_temp_file(parent: &Path) -> Result<(PathBuf, File), String> {
    create_unique_temp_file(parent, ".mediaforge-snapshot", "snapshot")
}

fn write_snapshot_manifest(
    output: &Path,
    records: &[SnapshotFileRecord],
    entries: &[SnapshotEntry],
    include_hidden: bool,
    include_generated: bool,
    hash_content: bool,
    overwrite: bool,
) -> Result<(), String> {
    const MAX_ENTRIES: usize = 10_000_000;
    const MAX_PATH_BYTES: usize = 1024 * 1024;
    if records.len() > MAX_ENTRIES {
        return Err(format!(
            "snapshot contains too many entries ({})",
            records.len()
        ));
    }
    if records.len() != entries.len() {
        return Err("snapshot entry count does not match the file scan".to_owned());
    }
    for record in records {
        if !snapshot_path_is_valid(&record.relative) || record.relative.len() > MAX_PATH_BYTES {
            return Err(format!(
                "cannot store invalid or oversized snapshot path: {:?}",
                record.relative
            ));
        }
    }
    if let Some(duplicate) = records
        .windows(2)
        .find(|pair| pair[0].relative == pair[1].relative)
    {
        return Err(format!(
            "multiple files resolve to the same portable path: {}",
            duplicate[0].relative
        ));
    }
    let parent = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let parent = fs::canonicalize(parent).map_err(|error| {
        format!(
            "cannot access snapshot directory {}: {error}",
            parent.display()
        )
    })?;
    let file_name = output
        .file_name()
        .ok_or_else(|| "snapshot output must name a file".to_owned())?;
    let output = parent.join(file_name);
    match entry_state(&output)
        .map_err(|error| format!("cannot inspect snapshot output: {error}"))?
    {
        EntryState::File if !overwrite => {
            return Err(format!(
                "snapshot already exists at {}; pass --overwrite to replace it",
                output.display()
            ));
        }
        EntryState::Other => {
            return Err(format!(
                "snapshot output is not a regular file: {}",
                output.display()
            ));
        }
        _ => {}
    }
    let (temporary, file) = snapshot_temp_file(&parent)?;
    let write_result = (|| -> Result<(), String> {
        let mut writer = BufWriter::with_capacity(64 * 1024, file);
        writer
            .write_all(SNAPSHOT_MAGIC)
            .map_err(|error| format!("could not write snapshot header: {error}"))?;
        let flags = u8::from(include_hidden)
            | (u8::from(include_generated) << 1)
            | (u8::from(hash_content) << 2);
        writer
            .write_all(&[flags])
            .and_then(|()| writer.write_all(&(records.len() as u64).to_le_bytes()))
            .map_err(|error| format!("could not write snapshot options: {error}"))?;
        for (record, entry) in records.iter().zip(entries) {
            let path = record.relative.as_bytes();
            let length = u32::try_from(path.len())
                .map_err(|_| format!("snapshot path is too long: {}", record.relative))?;
            writer
                .write_all(&length.to_le_bytes())
                .and_then(|()| writer.write_all(path))
                .and_then(|()| writer.write_all(&record.size.to_le_bytes()))
                .map_err(|error| format!("could not write snapshot entry: {error}"))?;
            match entry.modified_ns() {
                Some(value) => writer
                    .write_all(&[1])
                    .and_then(|()| writer.write_all(&value.to_le_bytes())),
                None => writer.write_all(&[0]),
            }
            .map_err(|error| format!("could not write snapshot timestamp: {error}"))?;
            if hash_content {
                writer
                    .write_all(&entry.content_hash)
                    .map_err(|error| format!("could not write snapshot content hash: {error}"))?;
            }
        }
        writer
            .flush()
            .and_then(|()| writer.get_ref().sync_all())
            .map_err(|error| format!("could not finalize snapshot data: {error}"))?;
        Ok(())
    })();
    if let Err(error) = write_result {
        let cleanup = fs::remove_file(&temporary);
        return Err(match cleanup {
            Ok(()) => error,
            Err(cleanup_error) if cleanup_error.kind() == io::ErrorKind::NotFound => error,
            Err(cleanup_error) => format!(
                "{error}; temporary snapshot remains at {}: {cleanup_error}",
                temporary.display()
            ),
        });
    }
    let publish = if overwrite {
        move_file_replace(&temporary, &output)
    } else {
        move_file_no_replace(&temporary, &output)
    };
    if let Err(error) = publish {
        let cleanup = fs::remove_file(&temporary);
        return Err(match cleanup {
            Ok(()) => format!("could not publish snapshot {}: {error}", output.display()),
            Err(cleanup_error) if cleanup_error.kind() == io::ErrorKind::NotFound => {
                format!("could not publish snapshot {}: {error}", output.display())
            }
            Err(cleanup_error) => format!(
                "could not publish snapshot {}: {error}; temporary file remains at {}: {cleanup_error}",
                output.display(),
                temporary.display()
            ),
        });
    }
    Ok(())
}

fn move_file_replace(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
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
        let result = unsafe {
            MoveFileExW(
                from.as_ptr(),
                to.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if result == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
    #[cfg(not(windows))]
    {
        fs::rename(from, to)
    }
}

fn write_snapshot_csv_row(output: &mut impl Write, values: &[String]) -> Result<(), String> {
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            output
                .write_all(b",")
                .map_err(|error| format!("could not write snapshot CSV: {error}"))?;
        }
        write_csv_field(output, value)
            .map_err(|error| format!("could not write snapshot CSV: {error}"))?;
    }
    output
        .write_all(b"\n")
        .map_err(|error| format!("could not write snapshot CSV: {error}"))
}

fn write_snapshot_csv_value(
    output: &mut impl Write,
    first: &mut bool,
    value: &str,
) -> io::Result<()> {
    if !*first {
        output.write_all(b",")?;
    }
    *first = false;
    write_csv_field(output, value)
}

fn write_snapshot_csv_display(
    output: &mut impl Write,
    first: &mut bool,
    value: impl std::fmt::Display,
) -> io::Result<()> {
    if !*first {
        output.write_all(b",")?;
    }
    *first = false;
    write!(output, "\"{value}\"")
}

fn write_snapshot_csv_optional_display(
    output: &mut impl Write,
    first: &mut bool,
    value: Option<impl std::fmt::Display>,
) -> io::Result<()> {
    match value {
        Some(value) => write_snapshot_csv_display(output, first, value),
        None => write_snapshot_csv_value(output, first, ""),
    }
}

fn write_snapshot_csv_event(
    output: &mut impl Write,
    change: &str,
    relative: &str,
    old: Option<SnapshotEntry>,
    new: Option<SnapshotEntry>,
) -> Result<(), String> {
    let size_delta = match (old, new) {
        (Some(old), Some(new)) => i128::from(new.size) - i128::from(old.size),
        (None, Some(new)) => i128::from(new.size),
        (Some(old), None) => -i128::from(old.size),
        (None, None) => 0,
    };
    let result = (|| -> io::Result<()> {
        let mut first = true;
        write_snapshot_csv_value(output, &mut first, change)?;
        write_snapshot_csv_value(output, &mut first, classify(Path::new(relative)).as_str())?;
        write_snapshot_csv_value(output, &mut first, relative)?;
        write_snapshot_csv_optional_display(output, &mut first, old.map(|entry| entry.size))?;
        write_snapshot_csv_optional_display(output, &mut first, new.map(|entry| entry.size))?;
        write_snapshot_csv_display(output, &mut first, size_delta)?;
        write_snapshot_csv_optional_display(
            output,
            &mut first,
            old.and_then(|entry| entry.modified_ns()),
        )?;
        write_snapshot_csv_optional_display(
            output,
            &mut first,
            new.and_then(|entry| entry.modified_ns()),
        )?;
        for _ in 8..16 {
            write_snapshot_csv_value(output, &mut first, "")?;
        }
        output.write_all(b"\n")
    })();
    result.map_err(|error| format!("could not write snapshot CSV: {error}"))
}

fn write_snapshot_json_optional_i128(
    output: &mut impl Write,
    value: Option<i128>,
) -> io::Result<()> {
    match value {
        Some(value) => write!(output, "{value}"),
        None => output.write_all(b"null"),
    }
}

fn write_snapshot_json_event(
    output: &mut impl Write,
    change: &str,
    relative: &str,
    old: Option<SnapshotEntry>,
    new: Option<SnapshotEntry>,
) -> Result<(), String> {
    let size_delta = match (old, new) {
        (Some(old), Some(new)) => i128::from(new.size) - i128::from(old.size),
        (None, Some(new)) => i128::from(new.size),
        (Some(old), None) => -i128::from(old.size),
        (None, None) => 0,
    };
    let result = (|| -> io::Result<()> {
        write!(output, "{{\"type\":{change:?},\"asset_type\":")?;
        write_json_string(output, classify(Path::new(relative)).as_str())?;
        output.write_all(b",\"path\":")?;
        write_json_string(output, relative)?;
        output.write_all(b",\"old_size_bytes\":")?;
        match old {
            Some(old) => write!(output, "{}", old.size)?,
            None => output.write_all(b"null")?,
        }
        output.write_all(b",\"new_size_bytes\":")?;
        match new {
            Some(new) => write!(output, "{}", new.size)?,
            None => output.write_all(b"null")?,
        }
        write!(
            output,
            ",\"size_delta_bytes\":{size_delta},\"old_modified_unix_ns\":"
        )?;
        write_snapshot_json_optional_i128(output, old.and_then(|entry| entry.modified_ns()))?;
        output.write_all(b",\"new_modified_unix_ns\":")?;
        write_snapshot_json_optional_i128(output, new.and_then(|entry| entry.modified_ns()))?;
        output.write_all(b"}\n")
    })();
    result.map_err(|error| format!("could not write snapshot JSON: {error}"))
}

fn run_snapshot(args: &[String]) -> Result<(), String> {
    let mut path = None;
    let mut output = None;
    let mut baseline = None;
    let mut json = false;
    let mut compact = false;
    let mut csv = false;
    let mut summary_only = false;
    let mut overwrite = false;
    let mut update_baseline = false;
    let mut fail_on_change = false;
    let mut include_hidden = None;
    let mut include_generated = None;
    let mut hash_content = None;
    let mut jobs = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .min(4);
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--output" | "--save" => {
                output = Some(PathBuf::from(
                    iter.next()
                        .ok_or_else(|| "--output requires a file path".to_owned())?,
                ));
            }
            "--diff" => {
                baseline =
                    Some(PathBuf::from(iter.next().ok_or_else(|| {
                        "--diff requires a snapshot file".to_owned()
                    })?));
            }
            "--json" => json = true,
            "--compact" => compact = true,
            "--csv" => csv = true,
            "--summary" => summary_only = true,
            "--overwrite" => overwrite = true,
            "--update" => update_baseline = true,
            "--fail-on-change" => fail_on_change = true,
            "--include-hidden" => include_hidden = Some(true),
            "--include-generated" => include_generated = Some(true),
            "--hash" => hash_content = Some(true),
            "--jobs" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--jobs requires a positive integer".to_owned())?;
                jobs = value
                    .parse::<usize>()
                    .map_err(|_| format!("invalid worker count '{value}'"))?;
                if jobs == 0 || jobs > 64 {
                    return Err("--jobs must be between 1 and 64".to_owned());
                }
            }
            "--help" | "-h" => {
                println!(
                    "Usage: mediaforge snapshot [PATH] (--output FILE [--overwrite] | --diff FILE [--update]) [--json|--csv] [--compact] [--summary] [--include-hidden] [--include-generated] [--hash] [--jobs N] [--fail-on-change]\n\nSave or compare project snapshots using relative paths, sizes, and modification times. --diff FILE --update replaces the baseline with the current project state after producing the diff. By default this is a fast metadata-only scan; --hash stores SHA-256 file fingerprints and compares contents, ignoring timestamp-only changes. --summary omits per-file diff rows while retaining totals, per-asset-type summaries, and five size-band summaries. --jobs controls concurrent file hashing (1–64; default up to 4). Unity/Unreal/Godot generated cache directories and hidden files are skipped by default. Snapshot files store inclusion settings and can be compared on another supported OS. --csv and --json select diff output formats. --compact removes formatting whitespace from JSON output and requires --json. --fail-on-change applies only to --diff."
                );
                return Ok(());
            }
            value if value.starts_with('-') => return Err(format!("unknown option: {value}")),
            value => {
                if path.is_some() {
                    return Err("snapshot accepts one directory path".to_owned());
                }
                path = Some(PathBuf::from(value));
            }
        }
    }
    if json && csv {
        return Err("--json and --csv cannot be used together".to_owned());
    }
    if compact && !json {
        return Err("--compact can only be used with --json".to_owned());
    }
    if csv && output.is_some() {
        return Err("--csv can only be used with --diff".to_owned());
    }
    if output.is_some() == baseline.is_some() {
        return Err("choose exactly one of --output FILE or --diff FILE".to_owned());
    }
    if overwrite && baseline.is_some() {
        return Err("--overwrite can only be used with --output".to_owned());
    }
    if update_baseline && baseline.is_none() {
        return Err("--update can only be used with --diff".to_owned());
    }
    if update_baseline && overwrite {
        return Err("--update and --overwrite cannot be used together".to_owned());
    }
    if fail_on_change && baseline.is_none() {
        return Err("--fail-on-change can only be used with --diff".to_owned());
    }
    if summary_only && baseline.is_none() {
        return Err("--summary can only be used with --diff".to_owned());
    }
    let path = path.unwrap_or_else(|| PathBuf::from("."));
    let root = fs::canonicalize(&path)
        .map_err(|error| format!("cannot access {}: {error}", path.display()))?;
    if !root.is_dir() {
        return Err(format!("expected a directory: {}", root.display()));
    }

    let mut old_entries = Vec::new();
    let mut excluded = None;
    let mut baseline_path = None;
    let (use_hidden, use_generated, use_hash) = if let Some(baseline) = &baseline {
        let baseline = fs::canonicalize(baseline)
            .map_err(|error| format!("cannot access snapshot {}: {error}", baseline.display()))?;
        if !is_real_file(&baseline) {
            return Err(format!(
                "snapshot is not a regular file: {}",
                baseline.display()
            ));
        }
        let (saved_hidden, saved_generated, saved_hash, entries) =
            read_snapshot_manifest(&baseline)?;
        if include_hidden.is_some_and(|value| value != saved_hidden)
            || include_generated.is_some_and(|value| value != saved_generated)
            || hash_content.is_some_and(|value| value != saved_hash)
        {
            return Err(
                "snapshot options differ from the baseline; rerun without inclusion overrides or create a matching baseline".to_owned(),
            );
        }
        excluded = Some(baseline);
        baseline_path = excluded.clone();
        old_entries = entries;
        (saved_hidden, saved_generated, saved_hash)
    } else {
        (
            include_hidden.unwrap_or(false),
            include_generated.unwrap_or(false),
            hash_content.unwrap_or(false),
        )
    };
    let output_abs = if let Some(output) = &output {
        let parent = output
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let parent = fs::canonicalize(parent).map_err(|error| {
            format!(
                "cannot access snapshot directory {}: {error}",
                parent.display()
            )
        })?;
        let file_name = output
            .file_name()
            .ok_or_else(|| "snapshot output must name a file".to_owned())?;
        Some(parent.join(file_name))
    } else {
        None
    };
    if let Some(output) = &output_abs {
        excluded = Some(output.clone());
    }
    let ignored = ignored_by_git(&root)?;
    let skip_dirs = if use_generated {
        SKIP_DIRS
    } else {
        SNAPSHOT_SKIP_DIRS
    };
    let mut records = Vec::<SnapshotFileRecord>::new();
    let filters = StrictWalkFilters {
        store_relative_path: true,
        ..StrictWalkFilters::default()
    };
    walk_strict_filtered_with_callback(
        &root,
        &root,
        use_hidden,
        skip_dirs,
        &ignored,
        &filters,
        |record| {
            records.push(SnapshotFileRecord {
                relative: record.relative,
                path: record.path,
                size: record.size,
                modified: record.modified,
            });
            Ok(())
        },
    )?;
    if let Some(excluded_relative) = excluded
        .as_deref()
        .and_then(|excluded| excluded.strip_prefix(&root).ok())
    {
        records.retain(|record| record.path != excluded_relative);
    }
    records.sort_unstable_by(|left, right| left.relative.cmp(&right.relative));
    let current_entries = collect_snapshot_entries(&root, &records, use_hash, jobs)?;
    if let Some(output) = output_abs {
        write_snapshot_manifest(
            &output,
            &records,
            &current_entries,
            use_hidden,
            use_generated,
            use_hash,
            overwrite,
        )?;
        let total_bytes = records
            .iter()
            .map(|record| u128::from(record.size))
            .sum::<u128>();
        if json {
            write_json_stdout("snapshot JSON", |writer| {
                let mut writer = JsonWhitespaceWriter::new(writer, compact);
                writeln!(
                    writer,
                    "{{\"type\":\"snapshot\",\"root\":{},\"output\":{},\"files\":{},\"bytes\":{total_bytes},\"include_hidden\":{use_hidden},\"include_generated\":{use_generated},\"hash\":{use_hash}}}",
                    json_string(&root.to_string_lossy()),
                    json_string(&output.to_string_lossy()),
                    records.len()
                )
            })?;
        } else {
            println!(
                "Saved snapshot {} ({} files, {})",
                output.display(),
                records.len(),
                human_size_wide(total_bytes)
            );
        }
        return Ok(());
    }

    let mut added = 0_usize;
    let mut removed = 0_usize;
    let mut modified = 0_usize;
    let mut unchanged = 0_usize;
    let mut added_bytes = 0_u128;
    let mut removed_bytes = 0_u128;
    let mut modified_size_delta_bytes = 0_i128;
    let mut by_type = BTreeMap::<&'static str, SnapshotTypeStats>::new();
    let mut by_size = BTreeMap::<&'static str, SnapshotSizeStats>::new();
    let mut old_cursor = 0;
    for (record, current) in records.iter().zip(&current_entries) {
        let relative = record.relative.as_str();
        let asset_type = classify(Path::new(relative)).as_str();
        let stats = by_type.entry(asset_type).or_default();
        match snapshot_entries_find_sorted(&old_entries, relative, &mut old_cursor) {
            None => {
                let size_stats = by_size.entry(size_category(current.size)).or_default();
                added += 1;
                stats.added += 1;
                added_bytes += u128::from(current.size);
                stats.added_bytes += u128::from(current.size);
                size_stats.added += 1;
                size_stats.added_bytes += u128::from(current.size);
            }
            Some(previous) if snapshot_entries_equal(*previous, *current, use_hash) => {
                unchanged += 1;
                stats.unchanged += 1;
            }
            Some(previous) => {
                let size_stats = by_size.entry(size_category(current.size)).or_default();
                modified += 1;
                stats.modified += 1;
                let size_delta = i128::from(current.size) - i128::from(previous.size);
                modified_size_delta_bytes += size_delta;
                stats.modified_size_delta_bytes += size_delta;
                size_stats.modified += 1;
                size_stats.modified_size_delta_bytes += size_delta;
            }
        }
    }
    let mut current_cursor = 0;
    for (relative, previous) in &old_entries {
        if !snapshot_records_contain_sorted_path(&records, relative, &mut current_cursor) {
            removed += 1;
            removed_bytes += u128::from(previous.size);
            let asset_type = classify(Path::new(relative)).as_str();
            let stats = by_type.entry(asset_type).or_default();
            stats.removed += 1;
            stats.removed_bytes += u128::from(previous.size);
            let size_stats = by_size.entry(size_category(previous.size)).or_default();
            size_stats.removed += 1;
            size_stats.removed_bytes += u128::from(previous.size);
        }
    }
    if update_baseline {
        let baseline = baseline_path
            .as_ref()
            .expect("--update requires a validated baseline");
        write_snapshot_manifest(
            baseline,
            &records,
            &current_entries,
            use_hidden,
            use_generated,
            use_hash,
            true,
        )?;
    }
    let mut ordered_size_summaries = by_size.iter().collect::<Vec<_>>();
    ordered_size_summaries.sort_by_key(|(bucket, _)| size_category_order(bucket));
    let change_count = added + removed + modified;
    if csv {
        let stdout = io::stdout();
        let mut stdout = io::BufWriter::with_capacity(64 * 1024, stdout.lock());
        write_snapshot_csv_row(
            &mut stdout,
            &[
                "change".to_owned(),
                "asset_type".to_owned(),
                "path".to_owned(),
                "old_size_bytes".to_owned(),
                "new_size_bytes".to_owned(),
                "size_delta_bytes".to_owned(),
                "old_modified_unix_ns".to_owned(),
                "new_modified_unix_ns".to_owned(),
                "added_count".to_owned(),
                "removed_count".to_owned(),
                "modified_count".to_owned(),
                "unchanged_count".to_owned(),
                "added_bytes".to_owned(),
                "removed_bytes".to_owned(),
                "modified_size_delta_bytes".to_owned(),
                "size_bucket".to_owned(),
            ],
        )?;
        if !summary_only {
            let mut old_cursor = 0;
            for (record, current) in records.iter().zip(&current_entries) {
                let relative = record.relative.as_str();
                let old =
                    snapshot_entries_find_sorted(&old_entries, relative, &mut old_cursor).copied();
                let change = match old {
                    None => "added",
                    Some(previous) if !snapshot_entries_equal(previous, *current, use_hash) => {
                        "modified"
                    }
                    Some(_) => continue,
                };
                write_snapshot_csv_event(&mut stdout, change, relative, old, Some(*current))?;
            }
            let mut current_cursor = 0;
            for (relative, old) in &old_entries {
                if !snapshot_records_contain_sorted_path(&records, relative, &mut current_cursor) {
                    write_snapshot_csv_event(&mut stdout, "removed", relative, Some(*old), None)?;
                }
            }
        }
        let mut summary = vec!["summary".to_owned(); 16];
        summary[8] = added.to_string();
        summary[9] = removed.to_string();
        summary[10] = modified.to_string();
        summary[11] = unchanged.to_string();
        summary[12] = added_bytes.to_string();
        summary[13] = removed_bytes.to_string();
        summary[14] = modified_size_delta_bytes.to_string();
        write_snapshot_csv_row(&mut stdout, &summary)?;
        for (asset_type, stats) in &by_type {
            let mut type_summary = vec!["type_summary".to_owned(); 16];
            type_summary[1] = (*asset_type).to_owned();
            type_summary[8] = stats.added.to_string();
            type_summary[9] = stats.removed.to_string();
            type_summary[10] = stats.modified.to_string();
            type_summary[11] = stats.unchanged.to_string();
            type_summary[12] = stats.added_bytes.to_string();
            type_summary[13] = stats.removed_bytes.to_string();
            type_summary[14] = stats.modified_size_delta_bytes.to_string();
            write_snapshot_csv_row(&mut stdout, &type_summary)?;
        }
        for (bucket, stats) in &ordered_size_summaries {
            let mut size_summary = vec!["size_summary".to_owned(); 16];
            size_summary[8] = stats.added.to_string();
            size_summary[9] = stats.removed.to_string();
            size_summary[10] = stats.modified.to_string();
            size_summary[12] = stats.added_bytes.to_string();
            size_summary[13] = stats.removed_bytes.to_string();
            size_summary[14] = stats.modified_size_delta_bytes.to_string();
            size_summary[15] = (**bucket).to_owned();
            write_snapshot_csv_row(&mut stdout, &size_summary)?;
        }
        stdout
            .flush()
            .map_err(|error| format!("could not write snapshot CSV: {error}"))?;
    } else if json {
        let stdout = io::stdout();
        let output = io::BufWriter::with_capacity(64 * 1024, stdout.lock());
        let mut output = JsonWhitespaceWriter::new(output, compact);
        if !summary_only {
            let mut old_cursor = 0;
            for (record, current) in records.iter().zip(&current_entries) {
                let relative = record.relative.as_str();
                let old = snapshot_entries_find_sorted(&old_entries, relative, &mut old_cursor);
                let kind = match old {
                    None => "added",
                    Some(previous) if !snapshot_entries_equal(*previous, *current, use_hash) => {
                        "modified"
                    }
                    Some(_) => continue,
                };
                write_snapshot_json_event(
                    &mut output,
                    kind,
                    relative,
                    old.copied(),
                    Some(*current),
                )?;
            }
            let mut current_cursor = 0;
            for (relative, old) in &old_entries {
                if snapshot_records_contain_sorted_path(&records, relative, &mut current_cursor) {
                    continue;
                }
                write_snapshot_json_event(&mut output, "removed", relative, Some(*old), None)?;
            }
        }
        let type_summary = by_type
            .iter()
            .map(|(asset_type, stats)| {
                format!(
                    "{{\"asset_type\":{},\"added\":{},\"removed\":{},\"modified\":{},\"unchanged\":{},\"added_bytes\":{},\"removed_bytes\":{},\"modified_size_delta_bytes\":{}}}",
                    json_string(asset_type),
                    stats.added,
                    stats.removed,
                    stats.modified,
                    stats.unchanged,
                    stats.added_bytes,
                    stats.removed_bytes,
                    stats.modified_size_delta_bytes
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let size_summary = ordered_size_summaries
            .iter()
            .map(|(bucket, stats)| {
                format!(
                    "{{\"size_bucket\":{},\"added\":{},\"removed\":{},\"modified\":{},\"added_bytes\":{},\"removed_bytes\":{},\"modified_size_delta_bytes\":{}}}",
                    json_string(bucket),
                    stats.added,
                    stats.removed,
                    stats.modified,
                    stats.added_bytes,
                    stats.removed_bytes,
                    stats.modified_size_delta_bytes
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        writeln!(
            output,
            "{{\"type\":\"summary\",\"files\":{},\"added\":{added},\"removed\":{removed},\"modified\":{modified},\"unchanged\":{unchanged},\"added_bytes\":{added_bytes},\"removed_bytes\":{removed_bytes},\"modified_size_delta_bytes\":{modified_size_delta_bytes},\"by_asset_type\":[{type_summary}],\"by_size_bucket\":[{size_summary}]}}",
            records.len()
        )
        .map_err(|error| format!("could not write snapshot JSON: {error}"))?;
        output
            .flush()
            .map_err(|error| format!("could not write snapshot JSON: {error}"))?;
    } else {
        println!("Snapshot diff: {}", root.display());
        println!(
            "Added: {added} ({}) · Removed: {removed} ({}) · Modified: {modified} · Unchanged: {unchanged}",
            human_size_wide(added_bytes),
            human_size_wide(removed_bytes)
        );
        println!("Net modified size: {modified_size_delta_bytes} bytes");
        for (asset_type, stats) in &by_type {
            println!(
                "  {asset_type:<9} +{} -{} ~{} ={}",
                stats.added, stats.removed, stats.modified, stats.unchanged
            );
        }
        for (bucket, stats) in &ordered_size_summaries {
            println!(
                "  {bucket:<18} +{} ({}) -{} ({}) ~{} (net {:+} B)",
                stats.added,
                human_size_wide(stats.added_bytes),
                stats.removed,
                human_size_wide(stats.removed_bytes),
                stats.modified,
                stats.modified_size_delta_bytes
            );
        }
        if !summary_only {
            let mut old_cursor = 0;
            for (record, current) in records.iter().zip(&current_entries) {
                let relative = record.relative.as_str();
                let asset_type = classify(Path::new(relative)).as_str();
                match snapshot_entries_find_sorted(&old_entries, relative, &mut old_cursor) {
                    None => println!(
                        "  added    {asset_type:<9} {:>10}  {relative}",
                        human_size(current.size)
                    ),
                    Some(previous) if previous != current => println!(
                        "  modified {asset_type:<9} {:>10} -> {:<10} {relative}",
                        human_size(previous.size),
                        human_size(current.size)
                    ),
                    Some(_) => {}
                }
            }
            let mut current_cursor = 0;
            for (relative, previous) in &old_entries {
                if !snapshot_records_contain_sorted_path(&records, relative, &mut current_cursor) {
                    println!(
                        "  removed  {:<9} {:>10}  {relative}",
                        classify(Path::new(relative)).as_str(),
                        human_size(previous.size)
                    );
                }
            }
        }
    }
    if fail_on_change && change_count > 0 {
        return Err(format!("snapshot found {change_count} change(s)"));
    }
    Ok(())
}

fn parse_size(value: &str) -> Result<u64, String> {
    let trimmed = value.trim();
    let split = trimmed
        .find(|ch: char| !ch.is_ascii_digit() && ch != '.')
        .unwrap_or(trimmed.len());
    let (number, unit) = trimmed.split_at(split);
    let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
    if whole.is_empty() && fraction.is_empty()
        || !whole.bytes().all(|digit| digit.is_ascii_digit())
        || !fraction.bytes().all(|digit| digit.is_ascii_digit())
    {
        return Err(format!("invalid size: {value}"));
    }
    let unit = unit.trim();
    let multiplier = match unit {
        "" => 1_u128,
        value if value.eq_ignore_ascii_case("b") => 1,
        value
            if value.eq_ignore_ascii_case("k")
                || value.eq_ignore_ascii_case("kb")
                || value.eq_ignore_ascii_case("kib") =>
        {
            1024
        }
        value
            if value.eq_ignore_ascii_case("m")
                || value.eq_ignore_ascii_case("mb")
                || value.eq_ignore_ascii_case("mib") =>
        {
            1024_u128.pow(2)
        }
        value
            if value.eq_ignore_ascii_case("g")
                || value.eq_ignore_ascii_case("gb")
                || value.eq_ignore_ascii_case("gib") =>
        {
            1024_u128.pow(3)
        }
        value
            if value.eq_ignore_ascii_case("t")
                || value.eq_ignore_ascii_case("tb")
                || value.eq_ignore_ascii_case("tib") =>
        {
            1024_u128.pow(4)
        }
        other => {
            return Err(format!(
                "unknown size unit '{}' in '{value}'",
                other.to_ascii_lowercase()
            ));
        }
    };
    let whole: u128 = if whole.is_empty() {
        0
    } else {
        whole
            .parse()
            .map_err(|_| format!("size is too large: {value}"))?
    };
    let whole_bytes = whole
        .checked_mul(multiplier)
        .ok_or_else(|| format!("size is too large: {value}"))?;
    let fractional_bytes = fraction_ceil_scaled(fraction, multiplier);
    let bytes = whole_bytes
        .checked_add(fractional_bytes)
        .ok_or_else(|| format!("size is too large: {value}"))?;
    if bytes > u128::from(u64::MAX) {
        return Err(format!("size is too large: {value}"));
    }
    Ok(bytes as u64)
}

fn parse_duration_seconds(value: &str) -> Result<u64, String> {
    let trimmed = value.trim();
    let split = trimmed
        .find(|ch: char| !ch.is_ascii_digit())
        .unwrap_or(trimmed.len());
    let (amount, unit) = trimmed.split_at(split);
    if amount.is_empty() || !amount.bytes().all(|digit| digit.is_ascii_digit()) {
        return Err(
            "use a positive integer followed by s, m, h, d, or w (for example 12h)".to_owned(),
        );
    }
    let amount = amount
        .parse::<u64>()
        .map_err(|_| "duration is too large".to_owned())?;
    if amount == 0 {
        return Err("duration must be greater than zero".to_owned());
    }
    let seconds_per_unit = match unit {
        unit if unit.eq_ignore_ascii_case("s") => 1,
        unit if unit.eq_ignore_ascii_case("m") => 60,
        unit if unit.eq_ignore_ascii_case("h") => 60 * 60,
        unit if unit.eq_ignore_ascii_case("d") => 24 * 60 * 60,
        unit if unit.eq_ignore_ascii_case("w") => 7 * 24 * 60 * 60,
        _ => return Err("use s, m, h, d, or w (for example 12h)".to_owned()),
    };
    amount
        .checked_mul(seconds_per_unit)
        .ok_or_else(|| "duration is too large".to_owned())
}

fn fraction_ceil_scaled(fraction: &str, multiplier: u128) -> u128 {
    if fraction.is_empty() {
        return 0;
    }
    let multiplier = multiplier as u64;
    let mut carry = 0_u64;
    let mut has_remainder = false;
    for digit in fraction.bytes().rev() {
        let product = u64::from(digit - b'0') * multiplier + carry;
        has_remainder |= !product.is_multiple_of(10);
        carry = product / 10;
    }
    u128::from(carry) + u128::from(has_remainder)
}

fn print_help() {
    println!(
        "MediaForge {VERSION}\nFast, local-first CLI for game and video production assets.\n\nUsage:\n  mediaforge doctor [--json] [--strict]\n  mediaforge scan-config <init|validate|show> [FILE] [--json]\n  mediaforge scan [PATH] [--json|--csv] [--compact] [--csv-info] [--show-filters] [--include-hidden] [--strict] [--no-same-size] [--type TYPE] [--min-size SIZE] [--max-size SIZE] [--modified-since UNIX_SECONDS] [--modified-until UNIX_SECONDS] [--modified-within DURATION] [--path-contains TEXT ...] [--exclude-path-contains TEXT ...] [--extension EXTENSIONS ...] [--largest N] [--sort path|size|modified] [--settings FILE]\n  mediaforge dupes [PATH] [--json] [--compact] [--summary] [--include-hidden] [--exclude-path-contains TEXT ...] [--type TYPE] [--min-size SIZE] [--max-size SIZE] [--extension EXTENSIONS] [--top-groups N] [--jobs N]\n  mediaforge probe <FILE> [--compact] [--summary]\n  mediaforge probe-dir <PATH> [--extension EXTENSIONS] [--include-hidden] [--type video|audio] [--min-size SIZE] [--max-size SIZE] [--jobs N] [--summary] [--compare] [--compare-summary] [--csv]\n  mediaforge storyboard <VIDEO> --output <IMAGE> [--interval SECONDS] [--columns N] [--rows N] [--width PIXELS]\n  mediaforge scene-detect-dir <PATH> [--threshold 0..1] [--min-gap SECONDS] [--segments] [--summary] [--fps RATE] [--extension EXTENSIONS] [--include-hidden] [--jobs N]\n  mediaforge scene-detect <VIDEO> [--threshold 0..1] [--min-gap SECONDS] [--segments] [--summary] [--format jsonl|csv] [--fps RATE]\n  mediaforge proxy <VIDEO> --output <FILE.mp4> [--profile fast|balanced|quality] [--settings FILE] [--max-width PIXELS] [--crf N] [--preset PRESET]\n  mediaforge proxy-dir <PATH> --output-dir <DIR> [--extension EXTENSIONS] [--include-hidden] [--profile fast|balanced|quality] [--settings FILE] [--max-width PIXELS] [--crf N] [--preset PRESET] [--jobs N] [--skip-existing] [--json] [--summary]\n  mediaforge proxy-config init [FILE] [--profile fast|balanced|quality] [--max-width PIXELS] [--crf N] [--preset PRESET]\n  mediaforge extract-audio <VIDEO> --output <FILE> [--format mp3|wav|flac] [--bitrate RATE] [--stream N]\n  mediaforge extract-audio-dir <PATH> --output-dir <DIR> [--extension EXTENSIONS] [--include-hidden] [--format mp3|wav|flac] [--bitrate RATE] [--stream N] [--jobs N] [--skip-existing] [--json] [--summary]\n  mediaforge snapshot [PATH] (--output FILE [--overwrite] | --diff FILE [--update]) [--json|--csv] [--compact] [--summary] [--include-hidden] [--include-generated] [--hash] [--jobs N] [--fail-on-change]\n  mediaforge project [PATH] [--json] [--compact] [--no-cache-sizes] [--no-editor-files]\n  mediaforge unity-meta [PROJECT_PATH] [--json] [--compact] [--summary] [--sidecars-only] [--include-hidden]\n  mediaforge gitignore [PATH] [--apply] [--json] [--compact]\n  mediaforge organize [PATH] [--by type|extension|size] [--type TYPE] [--extension EXTENSIONS] [--min-size SIZE] [--include-hidden] [--include-generated] [--json] [--compact] [--summary] [--apply]\n  mediaforge rename [PATH] [--replace OLD NEW] [--prefix TEXT] [--suffix TEXT] [--extension-case keep|lower|upper] [--extension EXTENSIONS] [--include-hidden] [--json] [--compact] [--summary] [--apply]
  mediaforge sequence [PATH] [--prefix NAME] [--start N] [--padding N] [--extension EXTENSIONS] [--include-hidden] [--natural] [--json] [--compact] [--summary] [--apply]\n  mediaforge undo [PATH] [--json] [--compact]\n  mediaforge pack <PATH> --output <FILE.tar> [--type TYPES] [--extension EXTENSIONS] [--min-size SIZE] [--max-size SIZE] [--exclude-path-contains TEXT ...] [--include-hidden] [--include-generated] [--dry-run] [--json] [--compact] [--list]\n  mediaforge --version\n  mediaforge --help"
    );
}

fn run_probe(args: &[String]) -> Result<(), String> {
    let mut path = None;
    let mut pretty = true;
    let mut summary = false;
    for arg in args {
        match arg.as_str() {
            "--compact" => pretty = false,
            "--summary" => summary = true,
            "--help" | "-h" => {
                println!(
                    "Usage: mediaforge probe <FILE> [--compact] [--summary]\n\nRead media metadata with ffprobe. --summary selects compact fields for size, duration, bitrate, codecs, dimensions, pixel/aspect formats, frame rates, and audio layout. --compact removes JSON whitespace."
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
    let metadata = fs::metadata(&path)
        .map_err(|error| format!("cannot access media file {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!("expected a media file: {}", path.display()));
    }
    let path = fs::canonicalize(&path)
        .map_err(|error| format!("cannot resolve media file {}: {error}", path.display()))?;

    let json = ffprobe_json(&path, pretty, summary, None)?;
    let mut stdout = io::stdout().lock();
    stdout
        .write_all(json.as_bytes())
        .map_err(|error| format!("could not write ffprobe output: {error}"))?;
    if !json.ends_with('\n') {
        stdout
            .write_all(b"\n")
            .map_err(|error| format!("could not finish ffprobe output: {error}"))?;
    }
    Ok(())
}

fn ffprobe_json(
    path: &Path,
    pretty: bool,
    summary: bool,
    stream_selector: Option<&str>,
) -> Result<String, String> {
    let mut command = Command::new("ffprobe");
    command.args(["-v", "error"]);
    if let Some(selector) = stream_selector {
        command.args(["-select_streams", selector]);
    }
    if summary {
        command.args([
            "-show_entries",
            "format=duration,format_name,size,bit_rate,nb_streams:stream=index,codec_type,codec_name,codec_tag_string,profile,width,height,pix_fmt,sample_aspect_ratio,display_aspect_ratio,duration,bit_rate,avg_frame_rate,r_frame_rate,nb_frames,sample_rate,channels,channel_layout",
        ]);
    } else {
        command.args(["-show_format", "-show_streams"]);
    }
    if pretty {
        command.args(["-of", "json"]);
    } else {
        command.args(["-of", "json=compact=1"]);
    }
    let output = command.arg(path).output().map_err(|error| {
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
    Ok(json)
}

fn skip_json_whitespace(bytes: &[u8], index: &mut usize) {
    while bytes.get(*index).is_some_and(u8::is_ascii_whitespace) {
        *index += 1;
    }
}

fn json_string_end(bytes: &[u8], start: usize) -> Result<usize, String> {
    if bytes.get(start) != Some(&b'"') {
        return Err("invalid JSON string in ffprobe output".to_owned());
    }
    let mut index = start + 1;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index = (index + 2).min(bytes.len()),
            b'"' => return Ok(index + 1),
            _ => index += 1,
        }
    }
    Err("unterminated JSON string in ffprobe output".to_owned())
}

fn collect_json_scalars(
    bytes: &[u8],
    index: &mut usize,
    prefix: &mut String,
    selected_indices: &HashMap<&str, usize>,
    fields: &mut [Option<String>],
    depth: usize,
) -> Result<(), String> {
    if depth > 64 {
        return Err("ffprobe JSON nesting is too deep".to_owned());
    }
    skip_json_whitespace(bytes, index);
    match bytes.get(*index).copied() {
        Some(b'{') => {
            *index += 1;
            loop {
                skip_json_whitespace(bytes, index);
                if bytes.get(*index) == Some(&b'}') {
                    *index += 1;
                    return Ok(());
                }
                let key_start = *index;
                let key_end = json_string_end(bytes, key_start)?;
                let key = std::str::from_utf8(&bytes[key_start + 1..key_end - 1])
                    .map_err(|error| format!("invalid JSON key in ffprobe output: {error}"))?;
                if key.contains('\\') {
                    return Err("unexpected escaped JSON key in ffprobe output".to_owned());
                }
                *index = key_end;
                skip_json_whitespace(bytes, index);
                if bytes.get(*index) != Some(&b':') {
                    return Err("invalid JSON object in ffprobe output".to_owned());
                }
                *index += 1;
                let previous_length = prefix.len();
                if previous_length > 0 {
                    prefix.push('.');
                }
                prefix.push_str(key);
                collect_json_scalars(bytes, index, prefix, selected_indices, fields, depth + 1)?;
                prefix.truncate(previous_length);
                skip_json_whitespace(bytes, index);
                match bytes.get(*index) {
                    Some(b',') => *index += 1,
                    Some(b'}') => {
                        *index += 1;
                        return Ok(());
                    }
                    _ => return Err("invalid JSON object separator in ffprobe output".to_owned()),
                }
            }
        }
        Some(b'[') => {
            *index += 1;
            let mut element = 0_usize;
            loop {
                skip_json_whitespace(bytes, index);
                if bytes.get(*index) == Some(&b']') {
                    *index += 1;
                    return Ok(());
                }
                let previous_length = prefix.len();
                prefix.push('[');
                write!(prefix, "{element}]").expect("writing a JSON array index to a String");
                collect_json_scalars(bytes, index, prefix, selected_indices, fields, depth + 1)?;
                prefix.truncate(previous_length);
                element += 1;
                skip_json_whitespace(bytes, index);
                match bytes.get(*index) {
                    Some(b',') => *index += 1,
                    Some(b']') => {
                        *index += 1;
                        return Ok(());
                    }
                    _ => return Err("invalid JSON array separator in ffprobe output".to_owned()),
                }
            }
        }
        Some(b'"') => {
            let start = *index;
            *index = json_string_end(bytes, start)?;
            if let Some(field_index) = selected_indices.get(prefix.as_str()) {
                fields[*field_index] =
                    Some(String::from_utf8_lossy(&bytes[start..*index]).into_owned());
            }
            Ok(())
        }
        Some(_) => {
            let start = *index;
            while bytes.get(*index).is_some_and(|byte| {
                !byte.is_ascii_whitespace() && !matches!(byte, b',' | b']' | b'}')
            }) {
                *index += 1;
            }
            if start == *index {
                return Err("invalid JSON scalar in ffprobe output".to_owned());
            }
            if let Some(field_index) = selected_indices.get(prefix.as_str()) {
                fields[*field_index] =
                    Some(String::from_utf8_lossy(&bytes[start..*index]).into_owned());
            }
            Ok(())
        }
        None => Err("truncated ffprobe JSON output".to_owned()),
    }
}

const PROBE_COMPARE_FIELDS: &[&str] = &[
    "format.format_name",
    "streams[0].codec_type",
    "streams[0].codec_name",
    "streams[0].codec_tag_string",
    "streams[0].profile",
    "streams[0].width",
    "streams[0].height",
    "streams[0].pix_fmt",
    "streams[0].sample_aspect_ratio",
    "streams[0].display_aspect_ratio",
    "streams[0].avg_frame_rate",
    "streams[0].r_frame_rate",
    "streams[0].sample_rate",
    "streams[0].channels",
    "streams[0].channel_layout",
];

enum ProbeRetainedData {
    CompareValues(Vec<String>),
}

struct ProbeCompareGroup {
    file_count: usize,
    paths: Option<Vec<usize>>,
}

fn probe_selected_values(probe: &str, selected_fields: &[&str]) -> Result<Vec<String>, String> {
    let selected_indices = selected_fields
        .iter()
        .enumerate()
        .map(|(index, field)| (*field, index))
        .collect::<HashMap<_, _>>();
    let mut fields = std::iter::repeat_with(|| None)
        .take(selected_fields.len())
        .collect::<Vec<Option<String>>>();
    let mut cursor = 0;
    collect_json_scalars(
        probe.as_bytes(),
        &mut cursor,
        &mut String::with_capacity(64),
        &selected_indices,
        &mut fields,
        0,
    )?;
    Ok(fields
        .into_iter()
        .map(|value| value.unwrap_or_else(|| "null".to_owned()))
        .collect())
}

const PROBE_CSV_FIELDS: &[&str] = &[
    "format.format_name",
    "format.duration",
    "streams[0].codec_type",
    "streams[0].codec_name",
    "streams[0].profile",
    "streams[0].width",
    "streams[0].height",
    "streams[0].avg_frame_rate",
    "streams[0].pix_fmt",
    "streams[0].sample_rate",
    "streams[0].channels",
    "streams[0].channel_layout",
    "format.size",
    "format.bit_rate",
];

fn summarize_probe_differences(
    records: &[FilePathRecord],
    results: &[(usize, bool, String, Option<ProbeRetainedData>)],
    include_paths: bool,
) -> Result<(), String> {
    let mut values = PROBE_COMPARE_FIELDS
        .iter()
        .map(|field| (*field, BTreeMap::<String, ProbeCompareGroup>::new()))
        .collect::<Vec<_>>();
    let mut failures = Vec::new();
    let mut successful = 0_usize;
    for (index, success, json_line, probe) in results {
        if !success {
            failures.push((*index, json_line.as_str()));
            continue;
        }
        successful += 1;
        let Some(ProbeRetainedData::CompareValues(fields)) = probe.as_ref() else {
            return Err("missing successful ffprobe comparison values".to_owned());
        };
        for (field_index, value) in fields.iter().enumerate() {
            let group = values[field_index]
                .1
                .entry(value.clone())
                .or_insert_with(|| ProbeCompareGroup {
                    file_count: 0,
                    paths: include_paths.then(Vec::new),
                });
            group.file_count += 1;
            if let Some(paths) = &mut group.paths {
                paths.push(*index);
            }
        }
    }
    let varying: Vec<_> = values
        .into_iter()
        .filter(|(_, values)| values.len() > 1)
        .collect();
    let stdout = io::stdout();
    let mut output = io::BufWriter::with_capacity(64 * 1024, stdout.lock());
    let failed = failures.len();
    let result = (|| -> io::Result<()> {
        write!(
            output,
            "{{\"files\":{},\"successful\":{successful},\"failed\":{failed},\"paths_included\":{include_paths},\"varying_fields\":[",
            records.len(),
        )?;
        for (field_index, (field, groups)) in varying.iter().enumerate() {
            if field_index > 0 {
                output.write_all(b",")?;
            }
            output.write_all(b"{\"field\":")?;
            write_json_string(&mut output, field)?;
            output.write_all(b",\"values\":[")?;
            for (value_index, (value, group)) in groups.iter().enumerate() {
                if value_index > 0 {
                    output.write_all(b",")?;
                }
                write!(
                    output,
                    "{{\"value\":{value},\"file_count\":{},\"files\":",
                    group.file_count
                )?;
                if let Some(paths) = &group.paths {
                    output.write_all(b"[")?;
                    for (path_index, record_index) in paths.iter().enumerate() {
                        if path_index > 0 {
                            output.write_all(b",")?;
                        }
                        write_json_string(&mut output, &records[*record_index].relative)?;
                    }
                    output.write_all(b"]}")?;
                } else {
                    output.write_all(b"null}")?;
                }
            }
            output.write_all(b"]}")?;
        }
        output.write_all(b"],\"failures\":[")?;
        for (failure_index, (index, error)) in failures.iter().enumerate() {
            if failure_index > 0 {
                output.write_all(b",")?;
            }
            output.write_all(b"{\"path\":")?;
            write_json_string(&mut output, &records[*index].relative)?;
            output.write_all(b",\"error\":")?;
            write_json_string(&mut output, error)?;
            output.write_all(b"}")?;
        }
        output.write_all(b"]}\n")?;
        output.flush()
    })();
    result.map_err(|error| format!("could not write comparison JSON: {error}"))?;
    Ok(())
}

fn probe_csv_value(value: Option<&str>) -> Cow<'_, str> {
    let Some(value) = value else {
        return Cow::Borrowed("");
    };
    if value == "null" {
        return Cow::Borrowed("");
    }
    let Some(value) = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
    else {
        return Cow::Borrowed(value);
    };
    let mut decoded = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(character) = chars.next() {
        if character != '\\' {
            decoded.push(character);
            continue;
        }
        match chars.next() {
            Some('"') => decoded.push('"'),
            Some('\\') => decoded.push('\\'),
            Some('/') => decoded.push('/'),
            Some('b') => decoded.push('\u{0008}'),
            Some('f') => decoded.push('\u{000c}'),
            Some('n') => decoded.push('\n'),
            Some('r') => decoded.push('\r'),
            Some('t') => decoded.push('\t'),
            Some(other) => {
                decoded.push('\\');
                decoded.push(other);
            }
            None => decoded.push('\\'),
        }
    }
    Cow::Owned(decoded)
}

fn print_probe_csv_row(
    output: &mut impl Write,
    path: &str,
    ok: bool,
    error: &str,
    values: Option<&[String]>,
) -> Result<(), String> {
    let mut first = true;
    let result = (|| -> io::Result<()> {
        write_csv_column(output, &mut first, path)?;
        write_csv_column(output, &mut first, if ok { "true" } else { "false" })?;
        write_csv_column(output, &mut first, error)?;
        for index in 0..PROBE_CSV_FIELDS.len() {
            let value = values
                .and_then(|values| values.get(index))
                .map(String::as_str);
            write_csv_column(output, &mut first, &probe_csv_value(value))?;
        }
        output.write_all(b"\n")
    })();
    result.map_err(|write_error| format!("could not write probe CSV row: {write_error}"))
}

fn probe_csv_batches(
    root: &Path,
    records: &[FilePathRecord],
    jobs: usize,
    selector: Option<&str>,
) -> Result<usize, String> {
    const BATCHES_PER_WORKER: usize = 4;
    let batch_size = jobs.saturating_mul(BATCHES_PER_WORKER).max(1);
    let stdout = io::stdout();
    let mut output = io::BufWriter::with_capacity(64 * 1024, stdout.lock());
    output
        .write_all(b"path,ok,error,format_name,duration_seconds,codec_type,codec,profile,width,height,frame_rate,pixel_format,sample_rate,channels,channel_layout,size_bytes,bit_rate\n")
        .map_err(|error| format!("could not write probe CSV header: {error}"))?;
    let mut failures = 0_usize;

    for batch_start in (0..records.len()).step_by(batch_size) {
        let batch_end = (batch_start + batch_size).min(records.len());
        let batch = &records[batch_start..batch_end];
        let next = std::sync::atomic::AtomicUsize::new(0);
        let next = &next;
        let results = std::sync::Mutex::new(
            Vec::<(usize, bool, String, Option<Vec<String>>)>::with_capacity(batch.len()),
        );
        let worker_count = jobs.min(batch.len());
        std::thread::scope(|scope| {
            for _ in 0..worker_count {
                scope.spawn(|| {
                    loop {
                        let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(record) = batch.get(index) else {
                            break;
                        };
                        let source_path = root.join(&record.path);
                        let (success, error, values) =
                            match ffprobe_json(&source_path, false, true, selector) {
                                Ok(probe) => {
                                    match probe_selected_values(&probe, PROBE_CSV_FIELDS) {
                                        Ok(values) => (true, String::new(), Some(values)),
                                        Err(error) => (false, error, None),
                                    }
                                }
                                Err(error) => (false, error, None),
                            };
                        results.lock().expect("probe results mutex poisoned").push((
                            batch_start + index,
                            success,
                            error,
                            values,
                        ));
                    }
                });
            }
        });

        let mut results = results
            .into_inner()
            .map_err(|_| "probe CSV results could not be collected".to_owned())?;
        results.sort_unstable_by_key(|(index, _, _, _)| *index);
        for (index, success, error, values) in results {
            failures += usize::from(!success);
            print_probe_csv_row(
                &mut output,
                &records[index].relative,
                success,
                &error,
                values.as_deref(),
            )?;
        }
    }

    output
        .flush()
        .map_err(|error| format!("could not write probe CSV: {error}"))?;
    Ok(failures)
}

fn run_probe_dir(args: &[String]) -> Result<(), String> {
    const MAX_PROBE_JOBS: usize = 32;
    let mut include_hidden = false;
    let mut summary = false;
    let mut compare = false;
    let mut compare_summary = false;
    let mut csv = false;
    let mut selected_kind = None;
    let mut extensions = None;
    let mut min_size = 0_u64;
    let mut max_size = None;
    let mut jobs = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .min(MAX_PROBE_JOBS);
    let mut path = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--include-hidden" => include_hidden = true,
            "--summary" => summary = true,
            "--csv" => csv = true,
            "--compare" => {
                compare = true;
                summary = true;
            }
            "--compare-summary" => {
                compare = true;
                compare_summary = true;
                summary = true;
            }
            "--type" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--type requires 'video' or 'audio'".to_owned())?;
                selected_kind = Some(match value.as_str() {
                    "video" => Kind::Video,
                    "audio" => Kind::Audio,
                    _ => return Err("--type must be 'video' or 'audio'".to_owned()),
                });
            }
            "--min-size" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--min-size requires a value".to_owned())?;
                min_size = parse_size(value)?;
            }
            "--max-size" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--max-size requires a value".to_owned())?;
                max_size = Some(parse_size(value)?);
            }
            "--extension" => {
                let value = iter.next().ok_or_else(|| {
                    "--extension requires a comma-separated extension list".to_owned()
                })?;
                extensions = Some(parse_extensions(value)?);
            }
            "--jobs" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--jobs requires a positive integer".to_owned())?;
                jobs = value.parse::<usize>().map_err(|_| {
                    format!("invalid --jobs value '{value}'; use a positive integer")
                })?;
                if jobs == 0 {
                    return Err("--jobs must be greater than zero".to_owned());
                }
                if jobs > MAX_PROBE_JOBS {
                    return Err(format!(
                        "--jobs is limited to {MAX_PROBE_JOBS} concurrent ffprobe processes"
                    ));
                }
            }
            "--help" | "-h" => {
                println!(
                    "Usage: mediaforge probe-dir <PATH> [--extension EXTENSIONS] [--include-hidden] [--type video|audio] [--min-size SIZE] [--max-size SIZE] [--jobs N] [--summary] [--compare] [--compare-summary] [--csv]\n\nProbe video and audio assets in a directory with ffprobe. Streams one JSON Lines result per file as probing finishes, with a completion counter. --csv writes one row per file with common video/audio metadata; failed probes are included with an error. --type filters to video or audio assets and selects only that stream type in the report. --min-size and --max-size filter file sizes before probing and accept bytes or K/M/G/T units. --jobs controls concurrent ffprobe processes (1–32). --summary selects compact fields. --compare implies --summary and emits only first-stream media-spec fields that differ across files, grouped by value. --compare-summary adds counts per value and omits path lists while retaining failures. Use --type to compare video or audio streams consistently."
                );
                return Ok(());
            }
            value if value.starts_with('-') => return Err(format!("unknown option: {value}")),
            value => {
                if path.is_some() {
                    return Err("probe-dir accepts one directory path".into());
                }
                path = Some(PathBuf::from(value));
            }
        }
    }
    if csv && compare {
        return Err("--csv cannot be combined with --compare".to_owned());
    }
    let path = path.ok_or_else(|| "probe-dir requires a directory path".to_owned())?;
    if max_size.is_some_and(|max_size| min_size > max_size) {
        return Err("--min-size cannot be greater than --max-size".to_owned());
    }
    let root = fs::canonicalize(&path)
        .map_err(|error| format!("cannot access {}: {error}", path.display()))?;
    if !root.is_dir() {
        return Err(format!("expected a directory: {}", root.display()));
    }
    let ignored = ignored_by_git(&root)?;
    let mut records = Vec::<FilePathRecord>::new();
    let selected_kinds =
        selected_kind.map_or_else(|| vec![Kind::Video, Kind::Audio], |kind| vec![kind]);
    let filters = StrictWalkFilters {
        kinds: Some(&selected_kinds),
        extensions: extensions.as_ref(),
        sort_entries_per_directory: false,
        min_size: Some(min_size),
        max_size,
        excluded_subtree: None,
        exclude_path_contains_lower: None,
        omit_absolute_path: false,
        store_relative_path: true,
        omit_modified_time: true,
        omit_extension: true,
    };
    walk_strict_paths(
        &root,
        &root,
        include_hidden,
        SKIP_DIRS,
        &ignored,
        &filters,
        &mut records,
    )?;
    let stream_results = !compare;
    if !stream_results {
        records.sort_unstable_by(|left, right| left.relative.cmp(&right.relative));
    }
    if records.is_empty() {
        if csv {
            let stdout = io::stdout();
            writeln!(
                stdout.lock(),
                "path,ok,error,format_name,duration_seconds,codec_type,codec,profile,width,height,frame_rate,pixel_format,sample_rate,channels,channel_layout,size_bytes,bit_rate"
            )
            .map_err(|error| format!("could not write probe CSV header: {error}"))?;
        } else if compare {
            let stdout = io::stdout();
            writeln!(
                stdout.lock(),
                "{{\"files\":0,\"successful\":0,\"failed\":0,\"paths_included\":{},\"varying_fields\":[],\"failures\":[]}}",
                !compare_summary
            )
            .map_err(|error| format!("could not write probe comparison JSON: {error}"))?;
        }
        return Ok(());
    }

    if csv {
        let selector = selected_kind.map(|kind| match kind {
            Kind::Video => "v",
            Kind::Audio => "a",
            _ => unreachable!(),
        });
        let failures = probe_csv_batches(&root, &records, jobs, selector)?;
        if failures > 0 {
            return Err(format!("ffprobe failed for {failures} file(s)"));
        }
        return Ok(());
    }

    let next = std::sync::atomic::AtomicUsize::new(0);
    let completed = std::sync::atomic::AtomicUsize::new(0);
    let output_failed = std::sync::atomic::AtomicBool::new(false);
    let first_output_error = std::sync::Mutex::new(None::<String>);
    let stream_output = std::sync::Mutex::new(());
    let failures = std::sync::atomic::AtomicUsize::new(0);
    let retained_results = if stream_results { 0 } else { records.len() };
    let results = std::sync::Mutex::new(
        Vec::<(usize, bool, String, Option<ProbeRetainedData>)>::with_capacity(retained_results),
    );
    let worker_count = jobs.min(records.len());
    std::thread::scope(|scope| {
        for _ in 0..worker_count {
            scope.spawn(|| {
                loop {
                    if output_failed.load(std::sync::atomic::Ordering::Acquire) {
                        break;
                    }
                    let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(record) = records.get(index) else {
                        break;
                    };
                    if output_failed.load(std::sync::atomic::Ordering::Acquire) {
                        break;
                    }
                    let selector = selected_kind.map(|kind| match kind {
                        Kind::Video => "v",
                        Kind::Audio => "a",
                        _ => unreachable!(),
                    });
                    let source_path = root.join(&record.path);
                    let result = ffprobe_json(&source_path, false, summary, selector);
                    let (success, json, probe) = match result {
                        Ok(probe) if stream_results => {
                            let mut json = format!(
                                "{{\"path\":{},\"ok\":true,\"probe\":",
                                json_string(&record.relative)
                            );
                            let probe = probe.trim();
                            json.reserve(probe.len() + 1);
                            json.push_str(probe);
                            json.push('}');
                            (true, json, None)
                        }
                        Ok(probe) if compare => {
                            match probe_selected_values(&probe, PROBE_COMPARE_FIELDS) {
                                Ok(values) => (
                                    true,
                                    String::new(),
                                    Some(ProbeRetainedData::CompareValues(values)),
                                ),
                                Err(error) => (false, error, None),
                            }
                        }
                        Ok(_) => (true, String::new(), None),
                        Err(error) if stream_results => (
                            false,
                            format!(
                                "{{\"path\":{},\"ok\":false,\"error\":{}}}",
                                json_string(&record.relative),
                                json_string(&error)
                            ),
                            None,
                        ),
                        Err(error) => (false, error, None),
                    };
                    if !success {
                        failures.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    if stream_results {
                        let _guard = stream_output.lock().expect("probe output lock poisoned");
                        let current =
                            completed.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                        let stdout = io::stdout();
                        if let Err(error) = writeln!(
                            stdout.lock(),
                            "{{\"type\":\"result\",\"current\":{current},\"total\":{},{}",
                            records.len(),
                            &json[1..]
                        ) {
                            output_failed.store(true, std::sync::atomic::Ordering::Release);
                            first_output_error
                                .lock()
                                .expect("probe output error lock poisoned")
                                .get_or_insert_with(|| error.to_string());
                            break;
                        }
                    }
                    if !stream_results {
                        results
                            .lock()
                            .expect("probe result mutex poisoned")
                            .push((index, success, json, probe));
                    }
                }
            });
        }
    });
    if let Some(error) = first_output_error
        .into_inner()
        .map_err(|_| "probe output error could not be collected".to_owned())?
    {
        return Err(format!("could not write probe JSONL output: {error}"));
    }
    let mut results = results
        .into_inner()
        .map_err(|_| "probe results could not be collected".to_owned())?;
    if !stream_results {
        results.sort_by_key(|(index, _, _, _)| *index);
    }
    let failures = failures.load(std::sync::atomic::Ordering::Relaxed);
    if compare {
        summarize_probe_differences(&records, &results, !compare_summary)?;
    }
    if failures > 0 {
        return Err(format!("ffprobe failed for {failures} file(s)"));
    }
    Ok(())
}

fn run_storyboard(args: &[String]) -> Result<(), String> {
    let mut input = None;
    let mut output = None;
    let mut interval = 10_u64;
    let mut columns = 5_u32;
    let mut rows = 5_u32;
    let mut width = 320_u32;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--output" | "-o" => {
                output = Some(PathBuf::from(
                    iter.next()
                        .ok_or_else(|| "--output requires a file path".to_owned())?,
                ));
            }
            "--interval" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--interval requires seconds".to_owned())?;
                interval = value
                    .parse::<u64>()
                    .map_err(|_| format!("invalid interval '{value}'; use a positive integer"))?;
            }
            "--columns" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--columns requires a positive integer".to_owned())?;
                columns = value
                    .parse::<u32>()
                    .map_err(|_| format!("invalid column count '{value}'"))?;
            }
            "--rows" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--rows requires a positive integer".to_owned())?;
                rows = value
                    .parse::<u32>()
                    .map_err(|_| format!("invalid row count '{value}'"))?;
            }
            "--width" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--width requires pixels".to_owned())?;
                width = value
                    .parse::<u32>()
                    .map_err(|_| format!("invalid thumbnail width '{value}'"))?;
            }
            "--help" | "-h" => {
                println!(
                    "Usage: mediaforge storyboard <VIDEO> --output <IMAGE> [--interval SECONDS] [--columns N] [--rows N] [--width PIXELS]\n\nCreate a contact sheet with ffmpeg. Defaults: 10-second interval, 5 columns, 5 rows, 320-pixel thumbnails. Supports JPG, PNG, and WebP output; existing files are never overwritten."
                );
                return Ok(());
            }
            value if value.starts_with('-') => return Err(format!("unknown option: {value}")),
            value => {
                if input.is_some() {
                    return Err("storyboard accepts one video file".into());
                }
                input = Some(PathBuf::from(value));
            }
        }
    }
    let input = input.ok_or_else(|| "storyboard requires a video file".to_owned())?;
    let output = output.ok_or_else(|| "storyboard requires --output <IMAGE>".to_owned())?;
    if interval == 0 {
        return Err("--interval must be greater than zero".to_owned());
    }
    if columns == 0 || rows == 0 || width < 16 {
        return Err(
            "--columns and --rows must be positive, and --width must be at least 16".into(),
        );
    }
    let cells = columns
        .checked_mul(rows)
        .ok_or_else(|| "storyboard grid is too large".to_owned())?;
    if cells > 100 || width > 640 {
        return Err("storyboard is limited to 100 frames and 640 pixels per thumbnail".into());
    }
    let input = fs::canonicalize(&input)
        .map_err(|error| format!("cannot access video {}: {error}", input.display()))?;
    if !fs::metadata(&input)
        .map_err(|error| format!("cannot inspect video {}: {error}", input.display()))?
        .is_file()
    {
        return Err(format!("expected a video file: {}", input.display()));
    }
    let output_name = output
        .file_name()
        .ok_or_else(|| "output must name an image file".to_owned())?;
    let extension = output
        .extension()
        .and_then(|value| value.to_str())
        .ok_or_else(|| "output must use a .jpg, .png, or .webp extension".to_owned())?
        .to_ascii_lowercase();
    if !matches!(extension.as_str(), "jpg" | "jpeg" | "png" | "webp") {
        return Err("storyboard output must use a .jpg, .png, or .webp extension".to_owned());
    }
    let parent = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let parent = fs::canonicalize(parent).map_err(|error| {
        format!(
            "cannot access output directory {}: {error}",
            parent.display()
        )
    })?;
    let output = parent.join(output_name);
    if output == input {
        return Err("storyboard output must differ from the input video".to_owned());
    }
    if entry_state(&output)
        .map_err(|error| format!("cannot inspect output {}: {error}", output.display()))?
        != EntryState::Missing
    {
        return Err(format!(
            "output already exists; refusing to overwrite: {}",
            output.display()
        ));
    }

    let stem = output
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("storyboard");
    let temporary = create_unique_media_temp(&parent, stem, &extension)?;
    let tile_height = width * 9 / 16;
    let filter = format!(
        "fps=1/{interval},scale={width}:{tile_height}:force_original_aspect_ratio=decrease,pad={width}:{tile_height}:(ow-iw)/2:(oh-ih)/2,tile={columns}x{rows}"
    );
    let run_result = run_with_bounded_stderr(
        Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-n", "-i"])
            .arg(&input)
            .args(["-vf", &filter, "-frames:v", "1"])
            .arg(&temporary),
    );
    let (status, stderr) = match run_result {
        Ok(result) => result,
        Err(error) => {
            let message = if error.kind() == io::ErrorKind::NotFound {
                "ffmpeg was not found on PATH. Install FFmpeg to create storyboards.".to_owned()
            } else {
                format!("could not start ffmpeg: {error}")
            };
            return Err(message);
        }
    };
    if !status.success() {
        let detail = String::from_utf8_lossy(&stderr).trim().to_owned();
        let message = if detail.is_empty() {
            format!("ffmpeg failed with {status}")
        } else {
            format!("ffmpeg failed: {detail}")
        };
        return Err(remove_temp_with_context(&temporary, message));
    }
    let temporary_size = match fs::metadata(&temporary) {
        Ok(metadata) => metadata.len(),
        Err(error) => {
            return Err(remove_temp_with_context(
                &temporary,
                format!("could not inspect generated storyboard: {error}"),
            ));
        }
    };
    if temporary_size == 0 {
        return Err(remove_temp_with_context(
            &temporary,
            "ffmpeg produced an empty storyboard".to_owned(),
        ));
    }
    move_file_no_replace(&temporary, &output).map_err(|error| {
        remove_temp_with_context(
            &temporary,
            format!("could not publish storyboard {}: {error}", output.display()),
        )
    })?;
    println!(
        "Created storyboard {} ({}; up to {cells} frames)",
        output.display(),
        human_size(temporary_size)
    );
    Ok(())
}

#[derive(Clone, Copy)]
struct SceneCut {
    seconds: f64,
    score: f64,
}

struct SceneDetection {
    input: PathBuf,
    cuts: Vec<SceneCut>,
    cut_count: usize,
    duration: Option<f64>,
    fps: f64,
    nominal_fps: u32,
}

#[derive(Clone, Copy)]
struct VideoTiming {
    duration: Option<f64>,
    fps: f64,
}

fn scene_timecode(seconds: f64) -> String {
    let milliseconds = (seconds * 1000.0).round() as u64;
    let hours = milliseconds / 3_600_000;
    let minutes = (milliseconds / 60_000) % 60;
    let seconds = (milliseconds / 1000) % 60;
    let millis = milliseconds % 1000;
    format!("{hours:02}:{minutes:02}:{seconds:02}.{millis:03}")
}

fn write_scene_timecode_json(output: &mut impl Write, seconds: f64) -> io::Result<()> {
    let milliseconds = (seconds * 1000.0).round() as u64;
    write!(
        output,
        "\"{:02}:{:02}:{:02}.{:03}\"",
        milliseconds / 3_600_000,
        (milliseconds / 60_000) % 60,
        (milliseconds / 1000) % 60,
        milliseconds % 1000
    )
}

fn parse_frame_rate(value: &str) -> Option<f64> {
    let (numerator, denominator) = if let Some((numerator, denominator)) = value.split_once('/') {
        (
            numerator.parse::<f64>().ok()?,
            denominator.parse::<f64>().ok()?,
        )
    } else {
        (value.parse::<f64>().ok()?, 1.0)
    };
    let rate = numerator / denominator;
    (rate.is_finite() && rate > 0.0 && rate <= 1000.0).then_some(rate)
}

fn parse_requested_fps(value: &str) -> Result<f64, String> {
    parse_frame_rate(value).ok_or_else(|| {
        format!(
            "invalid frame rate '{value}'; use a positive number or rational such as 30000/1001"
        )
    })
}

fn probe_video_timing(path: &Path) -> Result<VideoTiming, String> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=avg_frame_rate,r_frame_rate,duration:format=duration",
            "-of",
            "default=noprint_wrappers=1",
        ])
        .arg(path)
        .output()
        .map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                "ffprobe was not found on PATH. Install FFmpeg to include video timing details."
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
    let mut duration = None;
    let mut average_rate = None;
    let mut nominal_rate = None;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Some((key, value)) = line.split_once('=') {
            match key {
                "duration" => {
                    if let Some(value) = value
                        .parse::<f64>()
                        .ok()
                        .filter(|value| value.is_finite() && *value > 0.0)
                    {
                        duration = Some(value);
                    }
                }
                "avg_frame_rate" => average_rate = parse_frame_rate(value),
                "r_frame_rate" => nominal_rate = parse_frame_rate(value),
                _ => {}
            }
        }
    }
    let fps = average_rate.or(nominal_rate).ok_or_else(|| {
        "ffprobe did not return a valid frame rate for the first video stream".to_owned()
    })?;
    Ok(VideoTiming { duration, fps })
}

fn detect_scene_cuts(
    input: &Path,
    threshold: f64,
    min_gap: f64,
    fps_override: Option<f64>,
    segments: bool,
    summary_only: bool,
) -> Result<SceneDetection, String> {
    let input = fs::canonicalize(input)
        .map_err(|error| format!("cannot access video {}: {error}", input.display()))?;
    if !fs::metadata(&input)
        .map_err(|error| format!("cannot inspect video {}: {error}", input.display()))?
        .is_file()
    {
        return Err(format!("expected a video file: {}", input.display()));
    }
    let timing = if fps_override.is_none() || segments {
        Some(probe_video_timing(&input)?)
    } else {
        None
    };
    let fps = fps_override
        .or_else(|| timing.map(|timing| timing.fps))
        .expect("video timing is probed when no FPS override is given");
    let nominal_fps = fps.round().max(1.0) as u32;
    let duration = if segments {
        Some(timing.and_then(|timing| timing.duration).ok_or_else(|| {
            "ffprobe did not return a valid video duration required by --segments".to_owned()
        })?)
    } else {
        None
    };
    let filter = format!("select='gt(scene,{threshold})',metadata=mode=print");
    let mut child = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "info",
            "-nostats",
            "-nostdin",
            "-i",
        ])
        .arg(&input)
        .args([
            "-map", "0:v:0", "-vf", &filter, "-an", "-sn", "-dn", "-f", "null", "-",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                "ffmpeg was not found on PATH. Install FFmpeg to detect scene changes.".to_owned()
            } else {
                format!("could not start ffmpeg: {error}")
            }
        })?;
    const ERROR_DETAIL_LIMIT: usize = 64 * 1024;
    let mut diagnostic_detail = Vec::new();
    let Some(stderr_pipe) = child.stderr.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err("could not capture ffmpeg diagnostics".to_owned());
    };
    let mut stderr = BufReader::with_capacity(64 * 1024, stderr_pipe);
    let mut line = Vec::with_capacity(256);
    let mut current_time = None;
    let mut cuts = Vec::<SceneCut>::new();
    let mut summary_last_cut = None::<SceneCut>;
    let mut cut_count = 0_usize;
    loop {
        line.clear();
        let bytes_read = loop {
            match stderr.read_until(b'\n', &mut line) {
                Ok(bytes_read) => break bytes_read,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("could not read ffmpeg diagnostics: {error}"));
                }
            }
        };
        if bytes_read == 0 {
            break;
        }
        if diagnostic_detail.len() < ERROR_DETAIL_LIMIT {
            let remaining = ERROR_DETAIL_LIMIT - diagnostic_detail.len();
            diagnostic_detail.extend_from_slice(&line[..line.len().min(remaining)]);
        }
        let line = String::from_utf8_lossy(&line);
        if let Some(value) = line.split_once("pts_time:").map(|(_, value)| value) {
            current_time = value
                .split_whitespace()
                .next()
                .and_then(|value| value.parse::<f64>().ok())
                .filter(|value| value.is_finite() && *value >= 0.0);
        }
        if let Some(value) = line
            .split_once("lavfi.scene_score=")
            .map(|(_, value)| value.trim())
        {
            let score = value
                .split_whitespace()
                .next()
                .and_then(|value| value.parse::<f64>().ok());
            if let (Some(seconds), Some(score)) = (current_time, score)
                && score.is_finite()
                && (0.0..=1.0).contains(&score)
            {
                let cut = SceneCut { seconds, score };
                if summary_only {
                    if let Some(previous) = summary_last_cut.as_mut()
                        && seconds - previous.seconds < min_gap
                    {
                        if score > previous.score {
                            *previous = cut;
                        }
                    } else {
                        summary_last_cut = Some(cut);
                        cut_count += 1;
                    }
                } else if let Some(previous) = cuts.last_mut()
                    && seconds - previous.seconds < min_gap
                {
                    if score > previous.score {
                        *previous = cut;
                    }
                } else {
                    cuts.push(cut);
                }
            }
        }
    }
    let status = child
        .wait()
        .map_err(|error| format!("could not wait for ffmpeg: {error}"))?;
    if !status.success() {
        let detail = String::from_utf8_lossy(&diagnostic_detail)
            .trim()
            .to_owned();
        return Err(if detail.is_empty() {
            format!("ffmpeg failed with {status}")
        } else {
            format!("ffmpeg failed: {detail}")
        });
    }
    Ok(SceneDetection {
        input,
        cut_count: if summary_only { cut_count } else { cuts.len() },
        cuts,
        duration,
        fps,
        nominal_fps,
    })
}

fn write_scene_detection_json(
    output: &mut impl Write,
    detection: &SceneDetection,
    threshold: f64,
    min_gap: f64,
    summary_only: bool,
) -> io::Result<()> {
    let source = json_string(&detection.input.to_string_lossy());
    if summary_only {
        writeln!(
            output,
            "{{\"type\":\"summary\",\"path\":{source},\"cuts\":{},\"duration_seconds\":{},\"fps\":{:.6},\"nominal_fps\":{},\"threshold\":{threshold:.6},\"min_gap_seconds\":{min_gap:.6},\"details_included\":false}}",
            detection.cut_count,
            detection
                .duration
                .map_or("null".to_owned(), |duration| format!("{duration:.6}")),
            detection.fps,
            detection.nominal_fps
        )?;
        return Ok(());
    }
    if let Some(duration) = detection.duration {
        let mut start = 0.0_f64;
        let mut segment_count = 0;
        for cut in &detection.cuts {
            if cut.seconds > start && cut.seconds < duration {
                segment_count += 1;
                write_scene_segment_json(
                    output,
                    &source,
                    segment_count,
                    start,
                    cut.seconds,
                    detection.fps,
                    detection.nominal_fps,
                )?;
                start = cut.seconds;
            }
        }
        if start < duration {
            segment_count += 1;
            write_scene_segment_json(
                output,
                &source,
                segment_count,
                start,
                duration,
                detection.fps,
                detection.nominal_fps,
            )?;
        }
        writeln!(
            output,
            "{{\"type\":\"summary\",\"path\":{source},\"segments\":{},\"scene_cuts\":{},\"duration_seconds\":{duration:.6},\"fps\":{:.6},\"nominal_fps\":{},\"threshold\":{threshold:.6},\"min_gap_seconds\":{min_gap:.6}}}",
            segment_count, detection.cut_count, detection.fps, detection.nominal_fps
        )?;
    } else {
        for cut in &detection.cuts {
            write!(
                output,
                "{{\"type\":\"scene\",\"path\":{source},\"timecode\":"
            )?;
            write_scene_timecode_json(output, cut.seconds)?;
            writeln!(
                output,
                ",\"time_seconds\":{:.6},\"frame\":{},\"fps\":{:.6},\"nominal_fps\":{},\"score\":{:.6}}}",
                cut.seconds,
                (cut.seconds * detection.fps).round() as u64,
                detection.fps,
                detection.nominal_fps,
                cut.score
            )?;
        }
        writeln!(
            output,
            "{{\"type\":\"summary\",\"path\":{source},\"cuts\":{},\"fps\":{:.6},\"nominal_fps\":{},\"threshold\":{threshold:.6},\"min_gap_seconds\":{min_gap:.6}}}",
            detection.cut_count, detection.fps, detection.nominal_fps
        )?;
    }
    Ok(())
}

fn write_scene_segment_json(
    output: &mut impl Write,
    source: &str,
    index: usize,
    start: f64,
    end: f64,
    fps: f64,
    nominal_fps: u32,
) -> io::Result<()> {
    write!(
        output,
        "{{\"type\":\"segment\",\"path\":{source},\"segment\":{index},\"start_timecode\":"
    )?;
    write_scene_timecode_json(output, start)?;
    write!(
        output,
        ",\"start_seconds\":{start:.6},\"start_frame\":{},\"end_timecode\":",
        (start * fps).round() as u64
    )?;
    write_scene_timecode_json(output, end)?;
    writeln!(
        output,
        ",\"end_seconds\":{end:.6},\"end_frame\":{},\"duration_seconds\":{:.6},\"fps\":{fps:.6},\"nominal_fps\":{nominal_fps}}}",
        (end * fps).round() as u64,
        end - start
    )
}

fn write_scene_segment_csv(
    output: &mut impl Write,
    input: &str,
    index: usize,
    start: f64,
    end: f64,
    fps: f64,
) -> io::Result<()> {
    write_csv_field(output, input)?;
    write!(output, ",{index},")?;
    write_csv_field(output, &scene_timecode(start))?;
    write!(output, ",{start:.6},{},", (start * fps).round() as u64)?;
    write_csv_field(output, &scene_timecode(end))?;
    writeln!(
        output,
        ",{end:.6},{},{:.6},{fps:.6}",
        (end * fps).round() as u64,
        end - start
    )
}

fn run_scene_detect(args: &[String]) -> Result<(), String> {
    let mut input = None;
    let mut threshold = 0.4_f64;
    let mut min_gap = 1.0_f64;
    let mut segments = false;
    let mut summary_only = false;
    let mut fps_override = None;
    let mut output_format = "jsonl";
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--threshold" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--threshold requires a value from 0 to 1".to_owned())?;
                threshold = value
                    .parse::<f64>()
                    .map_err(|_| format!("invalid scene threshold '{value}'"))?;
                if !threshold.is_finite() || !(0.0..=1.0).contains(&threshold) {
                    return Err("--threshold must be between 0 and 1".to_owned());
                }
            }
            "--min-gap" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--min-gap requires seconds".to_owned())?;
                min_gap = value
                    .parse::<f64>()
                    .map_err(|_| format!("invalid minimum gap '{value}'"))?;
                if !min_gap.is_finite() || min_gap < 0.0 {
                    return Err("--min-gap must be a non-negative number of seconds".to_owned());
                }
            }
            "--format" => {
                output_format = iter
                    .next()
                    .ok_or_else(|| "--format requires jsonl or csv".to_owned())?;
                if output_format != "jsonl" && output_format != "csv" {
                    return Err("--format must be 'jsonl' or 'csv'".to_owned());
                }
            }
            "--fps" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--fps requires a rate such as 30 or 30000/1001".to_owned())?;
                fps_override = Some(parse_requested_fps(value)?);
            }
            "--segments" => segments = true,
            "--summary" => summary_only = true,
            "--help" | "-h" => {
                println!(
                    "Usage: mediaforge scene-detect <VIDEO> [--threshold 0..1] [--min-gap SECONDS] [--segments] [--summary] [--format jsonl|csv] [--fps RATE]\n\nFind likely cuts with FFmpeg's scene score. Defaults: threshold 0.4 and at least 1 second between results. JSON Lines is the default; CSV writes a header and one row per cut. --segments turns cut points into contiguous start/end intervals spanning the video. --summary emits only the cut count and media timing as one JSON record, using bounded memory; it requires the default jsonl format. Frame numbers use the probed average rate; --fps overrides it (for example 30000/1001)."
                );
                return Ok(());
            }
            value if value.starts_with('-') => return Err(format!("unknown option: {value}")),
            value => {
                if input.is_some() {
                    return Err("scene-detect accepts one video file".to_owned());
                }
                input = Some(PathBuf::from(value));
            }
        }
    }
    if summary_only && output_format != "jsonl" {
        return Err("--summary requires --format jsonl".to_owned());
    }
    let input = input.ok_or_else(|| "scene-detect requires a video file".to_owned())?;
    let detection = detect_scene_cuts(
        &input,
        threshold,
        min_gap,
        fps_override,
        segments,
        summary_only,
    )?;
    let input = &detection.input;
    let cuts = &detection.cuts;
    let duration = detection.duration;
    let fps = detection.fps;
    let stdout = io::stdout();
    let mut output = io::BufWriter::with_capacity(64 * 1024, stdout.lock());
    let output_result = (|| -> io::Result<()> {
        if output_format == "csv" {
            if let Some(duration) = duration {
                let mut start = 0.0_f64;
                let mut segment_index = 0;
                let input_text = input.to_string_lossy();
                output.write_all(b"path,segment,start_timecode,start_seconds,start_frame,end_timecode,end_seconds,end_frame,duration_seconds,fps\n")?;
                for cut in cuts {
                    if cut.seconds > start && cut.seconds < duration {
                        segment_index += 1;
                        write_scene_segment_csv(
                            &mut output,
                            &input_text,
                            segment_index,
                            start,
                            cut.seconds,
                            fps,
                        )?;
                        start = cut.seconds;
                    }
                }
                if start < duration {
                    segment_index += 1;
                    write_scene_segment_csv(
                        &mut output,
                        &input_text,
                        segment_index,
                        start,
                        duration,
                        fps,
                    )?;
                }
            } else {
                output.write_all(b"path,timecode,time_seconds,frame,fps,score\n")?;
                for cut in cuts {
                    write_csv_field(&mut output, &input.to_string_lossy())?;
                    output.write_all(b",")?;
                    write_csv_field(&mut output, &scene_timecode(cut.seconds))?;
                    writeln!(
                        output,
                        ",{:.6},{},{:.6},{:.6}",
                        cut.seconds,
                        (cut.seconds * fps).round() as u64,
                        fps,
                        cut.score
                    )?;
                }
            }
        } else {
            write_scene_detection_json(&mut output, &detection, threshold, min_gap, summary_only)?;
        }
        Ok(())
    })();
    output_result.map_err(|error| format!("could not write scene detection output: {error}"))?;
    output
        .flush()
        .map_err(|error| format!("could not write scene detection output: {error}"))?;
    Ok(())
}

fn run_scene_detect_dir(args: &[String]) -> Result<(), String> {
    const MAX_JOBS: usize = 16;
    let mut root = None;
    let mut threshold = 0.4_f64;
    let mut min_gap = 1.0_f64;
    let mut segments = false;
    let mut summary_only = false;
    let mut include_hidden = false;
    let mut extensions = None;
    let mut fps = None;
    let mut jobs = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .min(4);
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--threshold" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--threshold requires a value from 0 to 1".to_owned())?;
                threshold = value
                    .parse::<f64>()
                    .map_err(|_| format!("invalid scene threshold '{value}'"))?;
                if !threshold.is_finite() || !(0.0..=1.0).contains(&threshold) {
                    return Err("--threshold must be between 0 and 1".to_owned());
                }
            }
            "--min-gap" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--min-gap requires seconds".to_owned())?;
                min_gap = value
                    .parse::<f64>()
                    .map_err(|_| format!("invalid minimum gap '{value}'"))?;
                if !min_gap.is_finite() || min_gap < 0.0 {
                    return Err("--min-gap must be a non-negative number of seconds".to_owned());
                }
            }
            "--fps" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--fps requires a rate such as 30 or 30000/1001".to_owned())?;
                fps = Some(parse_requested_fps(value)?);
            }
            "--extension" => {
                let value = iter.next().ok_or_else(|| {
                    "--extension requires a comma-separated extension list".to_owned()
                })?;
                extensions = Some(parse_extensions(value)?);
            }
            "--jobs" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--jobs requires a positive integer".to_owned())?;
                jobs = value
                    .parse::<usize>()
                    .map_err(|_| format!("invalid --jobs value '{value}'"))?;
                if jobs == 0 || jobs > MAX_JOBS {
                    return Err(format!("--jobs must be between 1 and {MAX_JOBS}"));
                }
            }
            "--segments" => segments = true,
            "--summary" => summary_only = true,
            "--include-hidden" => include_hidden = true,
            "--help" | "-h" => {
                println!(
                    "Usage: mediaforge scene-detect-dir <PATH> [--threshold 0..1] [--min-gap SECONDS] [--segments] [--summary] [--fps RATE] [--extension EXTENSIONS] [--include-hidden] [--jobs N]\n\nFind likely scene cuts in every video under a directory. Streams progress records and each video's JSON Lines results as processing finishes, followed by a batch summary. --segments emits contiguous intervals instead of individual cuts. --summary counts scene cuts but omits progress and per-cut/per-segment records, using bounded cut memory for long videos. --jobs controls concurrent FFmpeg processes (1–16; default 4)."
                );
                return Ok(());
            }
            value if value.starts_with('-') => return Err(format!("unknown option: {value}")),
            value => {
                if root.is_some() {
                    return Err("scene-detect-dir accepts one directory path".to_owned());
                }
                root = Some(PathBuf::from(value));
            }
        }
    }
    let root = root.ok_or_else(|| "scene-detect-dir requires a directory path".to_owned())?;
    let root = fs::canonicalize(&root)
        .map_err(|error| format!("cannot access {}: {error}", root.display()))?;
    if !root.is_dir() {
        return Err(format!("expected a directory: {}", root.display()));
    }
    let ignored = ignored_by_git(&root)?;
    let mut records = Vec::<FilePathRecord>::new();
    let kinds = [Kind::Video];
    let filters = StrictWalkFilters {
        kinds: Some(&kinds),
        extensions: extensions.as_ref(),
        omit_extension: true,
        omit_modified_time: true,
        store_relative_path: true,
        ..StrictWalkFilters::default()
    };
    walk_strict_paths(
        &root,
        &root,
        include_hidden,
        SKIP_DIRS,
        &ignored,
        &filters,
        &mut records,
    )?;

    let next = std::sync::atomic::AtomicUsize::new(0);
    let completed = std::sync::atomic::AtomicUsize::new(0);
    let output_failed = std::sync::atomic::AtomicBool::new(false);
    let stream_output = std::sync::Mutex::new(());
    let failed = std::sync::atomic::AtomicUsize::new(0);
    let first_output_error = std::sync::Mutex::new(None::<String>);
    std::thread::scope(|scope| {
        for _ in 0..jobs.min(records.len()) {
            scope.spawn(|| {
                loop {
                    if output_failed.load(std::sync::atomic::Ordering::Acquire) {
                        break;
                    }
                    let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(record) = records.get(index) else {
                        break;
                    };
                    if output_failed.load(std::sync::atomic::Ordering::Acquire) {
                        break;
                    }
                    let source_path = root.join(&record.path);
                    let result = detect_scene_cuts(
                        &source_path,
                        threshold,
                        min_gap,
                        fps,
                        segments,
                        summary_only,
                    );
                    let success = result.is_ok();
                    let write_error = {
                        let _guard = stream_output
                            .lock()
                            .expect("scene detection output lock poisoned");
                        let current = completed.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                            + 1;
                        let stdout = io::stdout();
                        let mut stdout = io::BufWriter::with_capacity(64 * 1024, stdout.lock());
                        let write_result = (|| -> io::Result<()> {
                            if !summary_only {
                                writeln!(
                                    stdout,
                                    "{{\"type\":\"progress\",\"current\":{current},\"total\":{},\"path\":{}}}",
                                    records.len(),
                                    json_string(&record.relative)
                                )?;
                            }
                            match result {
                                Ok(detection) => write_scene_detection_json(
                                    &mut stdout,
                                    &detection,
                                    threshold,
                                    min_gap,
                                    summary_only,
                                ),
                                Err(error) => writeln!(
                                    stdout,
                                    "{{\"type\":\"error\",\"path\":{},\"error\":{}}}",
                                    json_string(&source_path.to_string_lossy()),
                                    json_string(&format!("mediaforge: {error}"))
                                ),
                            }?;
                            stdout.flush()
                        })();
                        write_result.err().map(|error| error.to_string())
                    };
                    if !success {
                        failed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    if let Some(error) = write_error {
                        output_failed.store(true, std::sync::atomic::Ordering::Release);
                        let mut first_error = first_output_error
                            .lock()
                            .expect("scene detection output error mutex poisoned");
                        if first_error.is_none() {
                            *first_error = Some(error);
                        }
                    }
                }
            });
        }
    });
    if let Some(error) = first_output_error
        .into_inner()
        .map_err(|_| "scene detection output error could not be collected".to_owned())?
    {
        return Err(format!("could not write scene detection output: {error}"));
    }
    let failed = failed.load(std::sync::atomic::Ordering::Relaxed);
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    writeln!(
        stdout,
        "{{\"type\":\"batch_summary\",\"files\":{},\"successful\":{},\"failed\":{},\"threshold\":{threshold:.6},\"min_gap_seconds\":{min_gap:.6},\"segments\":{segments},\"fps_override\":{}}}",
        records.len(),
        records.len() - failed,
        failed,
        fps.map_or("null".to_owned(), |value| format!("{value:.6}"))
    )
    .map_err(|error| format!("could not write scene detection summary: {error}"))?;
    if failed > 0 {
        return Err(format!("scene detection failed for {failed} file(s)"));
    }
    Ok(())
}

fn proxy_profile_defaults(profile: &str) -> Result<(u32, u8, &'static str), String> {
    match profile {
        "fast" => Ok((960, 32, "ultrafast")),
        "balanced" => Ok((1280, 28, "veryfast")),
        "quality" => Ok((1920, 22, "fast")),
        _ => Err(format!(
            "unknown proxy profile '{profile}'; choose fast, balanced, or quality"
        )),
    }
}

#[derive(Default)]
struct ProxySettings {
    profile: Option<String>,
    max_width: Option<u32>,
    crf: Option<u8>,
    preset: Option<String>,
}

fn is_x264_preset(preset: &str) -> bool {
    matches!(
        preset,
        "ultrafast"
            | "superfast"
            | "veryfast"
            | "faster"
            | "fast"
            | "medium"
            | "slow"
            | "slower"
            | "veryslow"
    )
}

fn parse_proxy_settings(contents: &str, path: &Path) -> Result<ProxySettings, String> {
    let mut settings = ProxySettings::default();
    let mut seen = HashSet::new();
    for (index, raw_line) in contents.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line.split_once('=').ok_or_else(|| {
            format!(
                "invalid proxy settings at {}:{}; expected key=value",
                path.display(),
                index + 1
            )
        })?;
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();
        if value.is_empty() {
            return Err(format!(
                "empty proxy setting at {}:{}",
                path.display(),
                index + 1
            ));
        }
        if !seen.insert(key.clone()) {
            return Err(format!(
                "duplicate proxy setting '{key}' at {}:{}",
                path.display(),
                index + 1
            ));
        }
        match key.as_str() {
            "profile" => {
                proxy_profile_defaults(value).map_err(|error| {
                    format!(
                        "invalid proxy settings at {}:{}: {error}",
                        path.display(),
                        index + 1
                    )
                })?;
                settings.profile = Some(value.to_owned());
            }
            "max_width" => {
                let parsed = value.parse::<u32>().map_err(|_| {
                    format!("invalid max_width at {}:{}", path.display(), index + 1)
                })?;
                if !(16..=7680).contains(&parsed) {
                    return Err(format!(
                        "max_width at {}:{} must be between 16 and 7680",
                        path.display(),
                        index + 1
                    ));
                }
                settings.max_width = Some(parsed);
            }
            "crf" => {
                let parsed = value
                    .parse::<u8>()
                    .map_err(|_| format!("invalid crf at {}:{}", path.display(), index + 1))?;
                if parsed > 51 {
                    return Err(format!(
                        "crf at {}:{} must be between 0 and 51",
                        path.display(),
                        index + 1
                    ));
                }
                settings.crf = Some(parsed);
            }
            "preset" => {
                if !is_x264_preset(value) {
                    return Err(format!(
                        "invalid x264 preset '{value}' at {}:{}",
                        path.display(),
                        index + 1
                    ));
                }
                settings.preset = Some(value.to_owned());
            }
            _ => {
                return Err(format!(
                    "unknown proxy setting '{key}' at {}:{}",
                    path.display(),
                    index + 1
                ));
            }
        }
    }
    Ok(settings)
}

fn load_proxy_settings(path: Option<&Path>) -> Result<ProxySettings, String> {
    let automatic = path.is_none();
    let default_path = Path::new(".mediaforge-proxy");
    let path = path.unwrap_or(default_path);
    match fs::read_to_string(path) {
        Ok(contents) => parse_proxy_settings(&contents, path),
        Err(error) if automatic && error.kind() == io::ErrorKind::NotFound => {
            Ok(ProxySettings::default())
        }
        Err(error) => Err(format!(
            "cannot read proxy settings {}: {error}",
            path.display()
        )),
    }
}

fn resolve_proxy_settings(
    settings: ProxySettings,
    profile: Option<&str>,
    max_width: Option<u32>,
    crf: Option<u8>,
    preset: Option<String>,
) -> Result<(u32, u8, String), String> {
    let profile = profile
        .or(settings.profile.as_deref())
        .unwrap_or("balanced");
    let (profile_width, profile_crf, profile_preset) = proxy_profile_defaults(profile)?;
    let max_width = max_width.or(settings.max_width).unwrap_or(profile_width);
    let crf = crf.or(settings.crf).unwrap_or(profile_crf);
    let preset = preset
        .or(settings.preset)
        .unwrap_or_else(|| profile_preset.to_owned());
    if !(16..=7680).contains(&max_width) {
        return Err("--max-width must be between 16 and 7680 pixels".to_owned());
    }
    if crf > 51 {
        return Err("--crf must be between 0 and 51".to_owned());
    }
    if !is_x264_preset(&preset) {
        return Err(format!(
            "unsupported x264 preset '{preset}'; choose ultrafast, superfast, veryfast, faster, fast, medium, slow, slower, or veryslow"
        ));
    }
    Ok((max_width, crf, preset))
}

fn run_proxy_config(args: &[String]) -> Result<(), String> {
    let mut iter = args.iter();
    let action = iter
        .next()
        .ok_or_else(|| "proxy-config requires the 'init' action".to_owned())?;
    if action == "--help" || action == "-h" {
        println!(
            "Usage: mediaforge proxy-config init [FILE] [--profile fast|balanced|quality] [--max-width PIXELS] [--crf N] [--preset PRESET]\n\nCreate a proxy settings file without overwriting existing files. Defaults to .mediaforge-proxy in the current directory."
        );
        return Ok(());
    }
    if action != "init" {
        return Err(format!("unknown proxy-config action '{action}'; use init"));
    }
    let mut path = None;
    let mut profile = "balanced".to_owned();
    let mut max_width = None;
    let mut crf = None;
    let mut preset = None;
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--profile" => {
                profile = iter
                    .next()
                    .ok_or_else(|| "--profile requires fast, balanced, or quality".to_owned())?
                    .clone();
            }
            "--max-width" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--max-width requires pixels".to_owned())?;
                max_width = Some(
                    value
                        .parse::<u32>()
                        .map_err(|_| format!("invalid maximum width '{value}'"))?,
                );
            }
            "--crf" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--crf requires a value from 0 to 51".to_owned())?;
                crf = Some(
                    value
                        .parse::<u8>()
                        .map_err(|_| format!("invalid CRF '{value}'; use a value from 0 to 51"))?,
                );
            }
            "--preset" => {
                preset = Some(
                    iter.next()
                        .ok_or_else(|| "--preset requires an x264 preset".to_owned())?
                        .clone(),
                );
            }
            value if value.starts_with('-') => return Err(format!("unknown option: {value}")),
            value => {
                if path.is_some() {
                    return Err("proxy-config init accepts one file path".into());
                }
                path = Some(PathBuf::from(value));
            }
        }
    }
    let (width, resolved_crf, encoder_preset) = resolve_proxy_settings(
        ProxySettings::default(),
        Some(&profile),
        max_width,
        crf,
        preset.clone(),
    )?;
    let path = path.unwrap_or_else(|| PathBuf::from(".mediaforge-proxy"));
    let name = path
        .file_name()
        .ok_or_else(|| "settings path must name a file".to_owned())?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let parent = fs::canonicalize(parent).map_err(|error| {
        format!(
            "cannot access settings directory {}: {error}",
            parent.display()
        )
    })?;
    let path = parent.join(name);
    if entry_state(&path)
        .map_err(|error| format!("cannot inspect settings file {}: {error}", path.display()))?
        != EntryState::Missing
    {
        return Err(format!(
            "settings file already exists; refusing to overwrite: {}",
            path.display()
        ));
    }

    let mut contents = format!("# MediaForge proxy settings\nprofile={profile}\n");
    if let Some(max_width) = max_width {
        contents.push_str(&format!("max_width={max_width}\n"));
    }
    if let Some(crf) = crf {
        contents.push_str(&format!("crf={crf}\n"));
    }
    if let Some(preset) = &preset {
        contents.push_str(&format!("preset={preset}\n"));
    }
    write_new_file_atomically(&path, contents.as_bytes(), "proxy settings")?;
    println!(
        "Created proxy settings {} (max width {width}, CRF {resolved_crf}, preset {encoder_preset})",
        path.display()
    );
    Ok(())
}

fn run_proxy(args: &[String]) -> Result<(), String> {
    let mut input = None;
    let mut output = None;
    let mut profile = None;
    let mut settings_path = None;
    let mut max_width = None;
    let mut crf = None;
    let mut preset = None;
    let mut quiet = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--output" | "-o" => {
                output = Some(PathBuf::from(
                    iter.next()
                        .ok_or_else(|| "--output requires a file path".to_owned())?,
                ));
            }
            "--max-width" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--max-width requires pixels".to_owned())?;
                max_width = Some(
                    value
                        .parse::<u32>()
                        .map_err(|_| format!("invalid maximum width '{value}'"))?,
                );
            }
            "--crf" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--crf requires a value from 0 to 51".to_owned())?;
                crf = Some(
                    value
                        .parse::<u8>()
                        .map_err(|_| format!("invalid CRF '{value}'; use a value from 0 to 51"))?,
                );
            }
            "--profile" => {
                profile = Some(
                    iter.next()
                        .ok_or_else(|| "--profile requires fast, balanced, or quality".to_owned())?
                        .clone(),
                );
            }
            "--settings" => {
                settings_path =
                    Some(PathBuf::from(iter.next().ok_or_else(|| {
                        "--settings requires a file path".to_owned()
                    })?));
            }
            "--preset" => {
                preset = Some(
                    iter.next()
                        .ok_or_else(|| "--preset requires an x264 preset".to_owned())?
                        .clone(),
                );
            }
            "--quiet" => quiet = true,
            "--help" | "-h" => {
                println!(
                    "Usage: mediaforge proxy <VIDEO> --output <FILE.mp4> [--profile fast|balanced|quality] [--settings FILE] [--max-width PIXELS] [--crf N] [--preset PRESET]\n\nCreate an H.264/AAC editing proxy with ffmpeg. Automatically reads .mediaforge-proxy from the working directory when present. Explicit options override settings-file values. Existing files are never overwritten."
                );
                return Ok(());
            }
            value if value.starts_with('-') => return Err(format!("unknown option: {value}")),
            value => {
                if input.is_some() {
                    return Err("proxy accepts one video file".into());
                }
                input = Some(PathBuf::from(value));
            }
        }
    }
    let input = input.ok_or_else(|| "proxy requires a video file".to_owned())?;
    let output = output.ok_or_else(|| "proxy requires --output <FILE.mp4>".to_owned())?;
    let settings = load_proxy_settings(settings_path.as_deref())?;
    let (max_width, crf, preset) =
        resolve_proxy_settings(settings, profile.as_deref(), max_width, crf, preset)?;

    run_proxy_with_options(input, output, max_width, crf, &preset, quiet)
}

fn run_proxy_with_options(
    input: PathBuf,
    output: PathBuf,
    max_width: u32,
    crf: u8,
    preset: &str,
    quiet: bool,
) -> Result<(), String> {
    let input = fs::canonicalize(&input)
        .map_err(|error| format!("cannot access video {}: {error}", input.display()))?;
    if !fs::metadata(&input)
        .map_err(|error| format!("cannot inspect video {}: {error}", input.display()))?
        .is_file()
    {
        return Err(format!("expected a video file: {}", input.display()));
    }
    let output_name = output
        .file_name()
        .ok_or_else(|| "output must name an MP4 file".to_owned())?;
    if output
        .extension()
        .and_then(|value| value.to_str())
        .is_none_or(|value| !value.eq_ignore_ascii_case("mp4"))
    {
        return Err("proxy output must use the .mp4 extension".to_owned());
    }
    let parent = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let parent = fs::canonicalize(parent).map_err(|error| {
        format!(
            "cannot access output directory {}: {error}",
            parent.display()
        )
    })?;
    let output = parent.join(output_name);
    if output == input {
        return Err("proxy output must differ from the input video".to_owned());
    }
    if entry_state(&output)
        .map_err(|error| format!("cannot inspect output {}: {error}", output.display()))?
        != EntryState::Missing
    {
        return Err(format!(
            "output already exists; refusing to overwrite: {}",
            output.display()
        ));
    }

    let stem = output
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("proxy");
    let temporary = create_unique_media_temp(&parent, stem, "mp4")?;
    let filter = format!("scale=w='trunc(min({max_width},iw)/2)*2':h=-2");
    let run_result = run_with_bounded_stderr(
        Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-n", "-i"])
            .arg(&input)
            .args(["-map", "0:v:0", "-map", "0:a?", "-sn", "-dn", "-vf"])
            .arg(filter)
            .args(["-c:v", "libx264", "-preset"])
            .arg(preset)
            .args(["-crf"])
            .arg(crf.to_string())
            .args([
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
                "-b:a",
                "128k",
                "-movflags",
                "+faststart",
            ])
            .arg(&temporary),
    );
    let (status, stderr) = match run_result {
        Ok(result) => result,
        Err(error) => {
            let message = if error.kind() == io::ErrorKind::NotFound {
                "ffmpeg was not found on PATH. Install FFmpeg to create proxies.".to_owned()
            } else {
                format!("could not start ffmpeg: {error}")
            };
            return Err(message);
        }
    };
    if !status.success() {
        let detail = String::from_utf8_lossy(&stderr).trim().to_owned();
        let message = if detail.is_empty() {
            format!("ffmpeg failed with {status}")
        } else {
            format!("ffmpeg failed: {detail}")
        };
        return Err(remove_temp_with_context(&temporary, message));
    }
    let temporary_size = match fs::metadata(&temporary) {
        Ok(metadata) => metadata.len(),
        Err(error) => {
            return Err(remove_temp_with_context(
                &temporary,
                format!("could not inspect generated proxy: {error}"),
            ));
        }
    };
    if temporary_size == 0 {
        return Err(remove_temp_with_context(
            &temporary,
            "ffmpeg produced an empty proxy".to_owned(),
        ));
    }
    move_file_no_replace(&temporary, &output).map_err(|error| {
        remove_temp_with_context(
            &temporary,
            format!("could not publish proxy {}: {error}", output.display()),
        )
    })?;
    if !quiet {
        println!(
            "Created proxy {} ({})",
            output.display(),
            human_size(temporary_size)
        );
    }
    Ok(())
}

fn print_batch_result_json(
    current: usize,
    total: usize,
    path: &str,
    output: Option<&Path>,
    result: &Result<bool, String>,
) -> Result<(), String> {
    let stdout = io::stdout();
    let mut output_writer = stdout.lock();
    let write_result = (|| -> io::Result<()> {
        write!(
            output_writer,
            "{{\"type\":\"result\",\"current\":{current},\"total\":{total},\"path\":"
        )?;
        write_json_string(&mut output_writer, path)?;
        output_writer.write_all(b",\"output\":")?;
        if let Some(output) = output {
            write_json_string(&mut output_writer, &output.to_string_lossy())?;
        } else {
            output_writer.write_all(b"null")?;
        }
        match result {
            Ok(created) => writeln!(output_writer, ",\"ok\":true,\"skipped\":{}}}", !created),
            Err(error) => {
                output_writer.write_all(b",\"ok\":false,\"error\":")?;
                write_json_string(&mut output_writer, error)?;
                output_writer.write_all(b"}\n")
            }
        }
    })();
    write_result.map_err(|error| format!("could not write batch result JSON: {error}"))
}

fn run_proxy_dir(args: &[String]) -> Result<(), String> {
    const MAX_PROXY_JOBS: usize = 16;
    let mut path = None;
    let mut output_root = None;
    let mut extensions = None;
    let mut include_hidden = false;
    let mut profile = None;
    let mut settings_path = None;
    let mut max_width = None;
    let mut crf = None;
    let mut preset = None;
    let mut jobs = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .min(4);
    let mut skip_existing = false;
    let mut json = false;
    let mut summary_only = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--output-dir" => {
                output_root =
                    Some(PathBuf::from(iter.next().ok_or_else(|| {
                        "--output-dir requires a directory".to_owned()
                    })?));
            }
            "--extension" => {
                let value = iter.next().ok_or_else(|| {
                    "--extension requires a comma-separated extension list".to_owned()
                })?;
                extensions = Some(parse_extensions(value)?);
            }
            "--include-hidden" => include_hidden = true,
            "--json" => json = true,
            "--profile" => {
                profile = Some(
                    iter.next()
                        .ok_or_else(|| "--profile requires fast, balanced, or quality".to_owned())?
                        .clone(),
                );
            }
            "--settings" => {
                settings_path =
                    Some(PathBuf::from(iter.next().ok_or_else(|| {
                        "--settings requires a file path".to_owned()
                    })?));
            }
            "--max-width" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--max-width requires pixels".to_owned())?;
                max_width = Some(
                    value
                        .parse::<u32>()
                        .map_err(|_| format!("invalid maximum width '{value}'"))?,
                );
            }
            "--crf" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--crf requires a value from 0 to 51".to_owned())?;
                crf = Some(
                    value
                        .parse::<u8>()
                        .map_err(|_| format!("invalid CRF '{value}'; use a value from 0 to 51"))?,
                );
            }
            "--preset" => {
                preset = Some(
                    iter.next()
                        .ok_or_else(|| "--preset requires an x264 preset".to_owned())?
                        .clone(),
                );
            }
            "--jobs" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--jobs requires a positive integer".to_owned())?;
                jobs = value
                    .parse::<usize>()
                    .map_err(|_| format!("invalid --jobs value '{value}'"))?;
                if jobs == 0 {
                    return Err("--jobs must be greater than zero".to_owned());
                }
                if jobs > MAX_PROXY_JOBS {
                    return Err(format!(
                        "--jobs is limited to {MAX_PROXY_JOBS} concurrent ffmpeg processes"
                    ));
                }
            }
            "--skip-existing" => skip_existing = true,
            "--summary" => summary_only = true,
            "--help" | "-h" => {
                println!(
                    "Usage: mediaforge proxy-dir <PATH> --output-dir <DIR> [--extension EXTENSIONS] [--include-hidden] [--profile fast|balanced|quality] [--settings FILE] [--max-width PIXELS] [--crf N] [--preset PRESET] [--jobs N] [--skip-existing] [--json] [--summary]\n\nCreate H.264/AAC proxies for video assets while preserving their relative folders. Automatically reads .mediaforge-proxy from the working directory when present. Explicit options override settings-file values. --jobs controls concurrent ffmpeg processes (1–16; default 4). Existing regular output files are never overwritten; --skip-existing skips those files. Other occupied output paths are errors. Failed files are reported after the remaining videos are processed. The output directory must already exist. --json emits one result record per file as each job completes, with completion counters and a final summary as JSON Lines. --summary requires --json and emits failed-file records plus the final counts, omitting succeeded and skipped file records."
                );
                return Ok(());
            }
            value if value.starts_with('-') => return Err(format!("unknown option: {value}")),
            value => {
                if path.is_some() {
                    return Err("proxy-dir accepts one input directory".into());
                }
                path = Some(PathBuf::from(value));
            }
        }
    }
    if summary_only && !json {
        return Err("--summary can only be used with --json".to_owned());
    }
    let path = path.ok_or_else(|| "proxy-dir requires an input directory".to_owned())?;
    let output_root =
        output_root.ok_or_else(|| "proxy-dir requires --output-dir <DIR>".to_owned())?;
    let settings = load_proxy_settings(settings_path.as_deref())?;
    let (max_width, crf, preset) =
        resolve_proxy_settings(settings, profile.as_deref(), max_width, crf, preset)?;
    let root = fs::canonicalize(&path)
        .map_err(|error| format!("cannot access {}: {error}", path.display()))?;
    if !is_real_directory(&root) {
        return Err(format!("expected a real directory: {}", root.display()));
    }
    let output_root = fs::canonicalize(&output_root).map_err(|error| {
        format!(
            "cannot access output directory {}: {error}",
            output_root.display()
        )
    })?;
    if !is_real_directory(&output_root) {
        return Err(format!(
            "expected a real output directory: {}",
            output_root.display()
        ));
    }
    if root == output_root {
        return Err("output directory must differ from the input directory".to_owned());
    }
    if root.starts_with(&output_root) {
        return Err("output directory must not contain the input directory".to_owned());
    }
    let output_inside_root = output_root.starts_with(&root);
    let ignored = ignored_by_git(&root)?;
    let mut records = Vec::<FilePathRecord>::new();
    let kinds = [Kind::Video];
    let filters = StrictWalkFilters {
        kinds: Some(&kinds),
        extensions: extensions.as_ref(),
        excluded_subtree: output_inside_root.then_some(output_root.as_path()),
        omit_extension: true,
        omit_modified_time: true,
        store_relative_path: true,
        ..StrictWalkFilters::default()
    };
    walk_strict_paths(
        &root,
        &root,
        include_hidden,
        SKIP_DIRS,
        &ignored,
        &filters,
        &mut records,
    )?;
    records.sort_unstable_by(|left, right| left.relative.cmp(&right.relative));
    if records.is_empty() {
        if json {
            let stdout = io::stdout();
            writeln!(
                stdout.lock(),
                "{{\"type\":\"summary\",\"succeeded\":0,\"skipped\":0,\"failed\":0}}"
            )
            .map_err(|error| format!("could not write proxy summary JSON: {error}"))?;
        } else {
            println!("No video assets found.");
        }
        return Ok(());
    }

    let total = records.len();
    let mut plans: Vec<(Option<PathBuf>, Result<(), String>)> = records
        .iter()
        .map(|record| {
            let plan = (|| -> Result<PathBuf, String> {
                let relative = record.path.as_path();
                let stem = relative
                    .file_stem()
                    .and_then(|value| value.to_str())
                    .ok_or_else(|| "cannot create a proxy name from this path".to_owned())?;
                let mut relative_output = relative.to_path_buf();
                relative_output.set_file_name(format!("{stem}-proxy.mp4"));
                let output = output_root.join(relative_output);
                Ok(output)
            })();
            match plan {
                Ok(output) => (Some(output), Ok(())),
                Err(error) => (None, Err(error)),
            }
        })
        .collect();
    mark_output_path_collisions(&mut plans, "proxy");
    let next = std::sync::atomic::AtomicUsize::new(0);
    let completed = std::sync::atomic::AtomicUsize::new(0);
    let output_failed = std::sync::atomic::AtomicBool::new(false);
    let stream_error = std::sync::Mutex::new(None::<String>);
    let progress_output = std::sync::Mutex::new(());
    let succeeded_count = std::sync::atomic::AtomicUsize::new(0);
    let skipped_count = std::sync::atomic::AtomicUsize::new(0);
    let failed_count = std::sync::atomic::AtomicUsize::new(0);
    let outcomes = (!json).then(|| {
        (0..total)
            .map(|_| std::sync::atomic::AtomicU8::new(0))
            .collect::<Vec<_>>()
    });
    let errors = std::sync::Mutex::new(Vec::<(usize, String)>::new());
    std::thread::scope(|scope| {
        for _ in 0..jobs.min(total) {
            scope.spawn(|| {
                loop {
                    if output_failed.load(std::sync::atomic::Ordering::Acquire) {
                        break;
                    }
                    let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if index >= total {
                        break;
                    }
                    if output_failed.load(std::sync::atomic::Ordering::Acquire) {
                        break;
                    }
                    let record = &records[index];
                    let (output, plan) = &plans[index];
                    if !json {
                        println!("Proxying {}/{}: {}", index + 1, total, record.relative);
                    }
                    let result = plan.clone().and_then(|()| {
                        let output = output.as_ref().expect("successful plan has output");
                        let output_state = entry_state(output).map_err(|error| {
                            format!("cannot inspect output {}: {error}", output.display())
                        })?;
                        if output_state == EntryState::Other {
                            return Err(format!(
                                "output path exists but is not a regular file: {}",
                                output.display()
                            ));
                        }
                        if skip_existing && output_state == EntryState::File {
                            return Ok(false);
                        }
                        if let Some(parent) = output.parent() {
                            fs::create_dir_all(parent).map_err(|error| {
                                format!(
                                    "could not create proxy directory {}: {error}",
                                    parent.display()
                                )
                            })?;
                        }
                        run_proxy_with_options(
                            root.join(&record.path),
                            output.clone(),
                            max_width,
                            crf,
                            &preset,
                            true,
                        )
                        .map(|()| true)
                    });
                    if json {
                        let _guard = progress_output
                            .lock()
                            .expect("proxy progress output lock poisoned");
                        let current =
                            completed.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                        if (!summary_only || result.is_err())
                            && let Err(error) = print_batch_result_json(
                                current,
                                total,
                                &record.relative,
                                output.as_deref(),
                                &result,
                            )
                        {
                            output_failed.store(true, std::sync::atomic::Ordering::Release);
                            stream_error
                                .lock()
                                .expect("proxy output error lock poisoned")
                                .get_or_insert(error);
                            break;
                        }
                        match &result {
                            Ok(true) => {
                                succeeded_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            }
                            Ok(false) => {
                                skipped_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            }
                            Err(_) => {
                                failed_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            }
                        }
                    } else {
                        match result {
                            Ok(true) => outcomes.as_ref().expect("human output outcomes allocated")
                                [index]
                                .store(1, std::sync::atomic::Ordering::Relaxed),
                            Ok(false) => {
                                outcomes.as_ref().expect("human output outcomes allocated")[index]
                                    .store(2, std::sync::atomic::Ordering::Relaxed)
                            }
                            Err(error) => {
                                outcomes.as_ref().expect("human output outcomes allocated")[index]
                                    .store(3, std::sync::atomic::Ordering::Relaxed);
                                errors
                                    .lock()
                                    .expect("proxy error lock poisoned")
                                    .push((index, error));
                            }
                        }
                    }
                }
            });
        }
    });
    if let Some(error) = stream_error
        .into_inner()
        .map_err(|_| "proxy output error lock poisoned".to_owned())?
    {
        return Err(error);
    }
    let mut errors = errors
        .into_inner()
        .map_err(|_| "proxy error lock poisoned".to_owned())?;
    let (succeeded, skipped, failed) = if json {
        (
            succeeded_count.load(std::sync::atomic::Ordering::Relaxed),
            skipped_count.load(std::sync::atomic::Ordering::Relaxed),
            failed_count.load(std::sync::atomic::Ordering::Relaxed),
        )
    } else {
        errors.sort_unstable_by_key(|(index, _)| *index);
        let mut errors = errors.into_iter().peekable();
        let mut succeeded = 0_usize;
        let mut skipped = 0_usize;
        let mut failed = 0_usize;
        for (index, outcome) in outcomes
            .as_ref()
            .expect("human output outcomes allocated")
            .iter()
            .enumerate()
        {
            match outcome.load(std::sync::atomic::Ordering::Relaxed) {
                1 => {
                    succeeded += 1;
                    if let Some(output) = &plans[index].0 {
                        println!("Created proxy {}", output.display());
                    }
                }
                2 => {
                    skipped += 1;
                    if let Some(output) = &plans[index].0 {
                        println!("Skipped existing proxy {}", output.display());
                    }
                }
                3 => {
                    failed += 1;
                    let (error_index, error) = errors
                        .next()
                        .expect("failed proxy outcome has a recorded error");
                    debug_assert_eq!(error_index, index);
                    eprintln!("mediaforge: {}: {error}", records[index].relative);
                }
                _ => unreachable!("proxy outcome is recorded before workers finish"),
            }
        }
        (succeeded, skipped, failed)
    };
    if json {
        let stdout = io::stdout();
        writeln!(
            stdout.lock(),
            "{{\"type\":\"summary\",\"succeeded\":{succeeded},\"skipped\":{skipped},\"failed\":{failed}}}"
        )
        .map_err(|error| format!("could not write proxy summary JSON: {error}"))?;
    } else {
        println!(
            "Proxy batch finished: {succeeded} succeeded, {skipped} skipped, {failed} failed."
        );
    }
    if failed > 0 {
        return Err(format!("proxy generation failed for {failed} file(s)"));
    }
    Ok(())
}

fn run_extract_audio(args: &[String]) -> Result<(), String> {
    let mut input = None;
    let mut output = None;
    let mut format = "mp3".to_owned();
    let mut bitrate = None;
    let mut stream = 0_u32;
    let mut quiet = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--output" | "-o" => {
                output = Some(PathBuf::from(
                    iter.next()
                        .ok_or_else(|| "--output requires a file path".to_owned())?,
                ));
            }
            "--format" => {
                format = iter
                    .next()
                    .ok_or_else(|| "--format requires mp3, wav, or flac".to_owned())?
                    .to_ascii_lowercase();
            }
            "--bitrate" => {
                bitrate = Some(
                    iter.next()
                        .ok_or_else(|| "--bitrate requires a value such as 192k".to_owned())?
                        .clone(),
                );
            }
            "--stream" => {
                let value = iter.next().ok_or_else(|| {
                    "--stream requires a zero-based audio stream index".to_owned()
                })?;
                stream = value
                    .parse::<u32>()
                    .map_err(|_| format!("invalid audio stream index '{value}'"))?;
            }
            "--quiet" => quiet = true,
            "--help" | "-h" => {
                println!(
                    "Usage: mediaforge extract-audio <VIDEO> --output <FILE> [--format mp3|wav|flac] [--bitrate RATE] [--stream N]\n\nExtract an audio stream with ffmpeg. Defaults to MP3 at 192k from the first audio stream. --bitrate applies to MP3 only; --stream is a zero-based audio stream index. Existing files are never overwritten."
                );
                return Ok(());
            }
            value if value.starts_with('-') => return Err(format!("unknown option: {value}")),
            value => {
                if input.is_some() {
                    return Err("extract-audio accepts one video file".into());
                }
                input = Some(PathBuf::from(value));
            }
        }
    }
    let input = input.ok_or_else(|| "extract-audio requires a video file".to_owned())?;
    let output = output.ok_or_else(|| "extract-audio requires --output <FILE>".to_owned())?;
    if !matches!(format.as_str(), "mp3" | "wav" | "flac") {
        return Err("--format must be mp3, wav, or flac".to_owned());
    }
    if bitrate.is_some() && format != "mp3" {
        return Err("--bitrate can only be used with --format mp3".to_owned());
    }
    let bitrate = bitrate.unwrap_or_else(|| "192k".to_owned());
    const MP3_BITRATES: &[&str] = &["64k", "96k", "128k", "160k", "192k", "224k", "256k", "320k"];
    if format == "mp3" && !MP3_BITRATES.contains(&bitrate.as_str()) {
        return Err(format!(
            "unsupported MP3 bitrate '{bitrate}'; choose one of {}",
            MP3_BITRATES.join(", ")
        ));
    }

    run_extract_audio_with_options(input, output, &format, &bitrate, stream, quiet)
}

fn run_extract_audio_with_options(
    input: PathBuf,
    output: PathBuf,
    format: &str,
    bitrate: &str,
    stream: u32,
    quiet: bool,
) -> Result<(), String> {
    let input = fs::canonicalize(&input)
        .map_err(|error| format!("cannot access media file {}: {error}", input.display()))?;
    if !fs::metadata(&input)
        .map_err(|error| format!("cannot inspect media file {}: {error}", input.display()))?
        .is_file()
    {
        return Err(format!("expected a media file: {}", input.display()));
    }
    let output_name = output
        .file_name()
        .ok_or_else(|| "output must name an audio file".to_owned())?;
    if output
        .extension()
        .and_then(|value| value.to_str())
        .is_none_or(|value| !value.eq_ignore_ascii_case(format))
    {
        return Err(format!("output extension must match --format {format}"));
    }
    let parent = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let parent = fs::canonicalize(parent).map_err(|error| {
        format!(
            "cannot access output directory {}: {error}",
            parent.display()
        )
    })?;
    let output = parent.join(output_name);
    if output == input {
        return Err("audio output must differ from the input file".to_owned());
    }
    if entry_state(&output)
        .map_err(|error| format!("cannot inspect output {}: {error}", output.display()))?
        != EntryState::Missing
    {
        return Err(format!(
            "output already exists; refusing to overwrite: {}",
            output.display()
        ));
    }

    let stem = output
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("audio");
    let temporary = create_unique_media_temp(&parent, stem, format)?;
    let mut command = Command::new("ffmpeg");
    command
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-n", "-i"])
        .arg(&input)
        .args(["-map"])
        .arg(format!("0:a:{stream}"))
        .args(["-vn", "-sn", "-dn", "-c:a"]);
    match format {
        "mp3" => {
            command.arg("libmp3lame").args(["-b:a", bitrate]);
        }
        "wav" => {
            command.arg("pcm_s16le");
        }
        "flac" => {
            command.arg("flac");
        }
        _ => unreachable!(),
    }
    let run_result = run_with_bounded_stderr(command.arg(&temporary));
    let (status, stderr) = match run_result {
        Ok(result) => result,
        Err(error) => {
            let message = if error.kind() == io::ErrorKind::NotFound {
                "ffmpeg was not found on PATH. Install FFmpeg to extract audio.".to_owned()
            } else {
                format!("could not start ffmpeg: {error}")
            };
            return Err(message);
        }
    };
    if !status.success() {
        let detail = String::from_utf8_lossy(&stderr).trim().to_owned();
        let message = if detail.is_empty() {
            format!("ffmpeg failed with {status}")
        } else {
            format!("ffmpeg failed: {detail}")
        };
        return Err(remove_temp_with_context(&temporary, message));
    }
    let temporary_size = match fs::metadata(&temporary) {
        Ok(metadata) => metadata.len(),
        Err(error) => {
            return Err(remove_temp_with_context(
                &temporary,
                format!("could not inspect extracted audio: {error}"),
            ));
        }
    };
    if temporary_size == 0 {
        return Err(remove_temp_with_context(
            &temporary,
            "ffmpeg produced an empty audio file".to_owned(),
        ));
    }
    move_file_no_replace(&temporary, &output).map_err(|error| {
        remove_temp_with_context(
            &temporary,
            format!("could not publish audio {}: {error}", output.display()),
        )
    })?;
    if !quiet {
        println!(
            "Extracted audio {} ({})",
            output.display(),
            human_size(temporary_size)
        );
    }
    Ok(())
}

fn run_extract_audio_dir(args: &[String]) -> Result<(), String> {
    const MAX_AUDIO_JOBS: usize = 16;
    let mut path = None;
    let mut output_root = None;
    let mut extensions = None;
    let mut include_hidden = false;
    let mut format = "mp3".to_owned();
    let mut bitrate = None;
    let mut stream = 0_u32;
    let mut jobs = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .min(4);
    let mut skip_existing = false;
    let mut json = false;
    let mut summary_only = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--output-dir" => {
                output_root =
                    Some(PathBuf::from(iter.next().ok_or_else(|| {
                        "--output-dir requires a directory".to_owned()
                    })?));
            }
            "--extension" => {
                let value = iter.next().ok_or_else(|| {
                    "--extension requires a comma-separated extension list".to_owned()
                })?;
                extensions = Some(parse_extensions(value)?);
            }
            "--include-hidden" => include_hidden = true,
            "--format" => {
                format = iter
                    .next()
                    .ok_or_else(|| "--format requires mp3, wav, or flac".to_owned())?
                    .to_ascii_lowercase();
            }
            "--bitrate" => {
                bitrate = Some(
                    iter.next()
                        .ok_or_else(|| "--bitrate requires a value such as 192k".to_owned())?
                        .clone(),
                );
            }
            "--stream" => {
                let value = iter.next().ok_or_else(|| {
                    "--stream requires a zero-based audio stream index".to_owned()
                })?;
                stream = value
                    .parse::<u32>()
                    .map_err(|_| format!("invalid audio stream index '{value}'"))?;
            }
            "--jobs" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--jobs requires a positive integer".to_owned())?;
                jobs = value
                    .parse::<usize>()
                    .map_err(|_| format!("invalid --jobs value '{value}'"))?;
                if jobs == 0 {
                    return Err("--jobs must be greater than zero".to_owned());
                }
                if jobs > MAX_AUDIO_JOBS {
                    return Err(format!(
                        "--jobs is limited to {MAX_AUDIO_JOBS} concurrent ffmpeg processes"
                    ));
                }
            }
            "--skip-existing" => skip_existing = true,
            "--json" => json = true,
            "--summary" => summary_only = true,
            "--help" | "-h" => {
                println!(
                    "Usage: mediaforge extract-audio-dir <PATH> --output-dir <DIR> [--extension EXTENSIONS] [--include-hidden] [--format mp3|wav|flac] [--bitrate RATE] [--stream N] [--jobs N] [--skip-existing] [--json] [--summary]\n\nExtract audio from video and audio assets while preserving relative folders. Defaults to MP3 at 192k from the first audio stream. --jobs controls concurrent ffmpeg processes (1–16; default 4). Existing regular output files are never overwritten; --skip-existing skips those files. Other occupied output paths are errors. The output directory must already exist. --json emits one result record per file as each job completes, with completion counters and a final summary as JSON Lines. --summary requires --json and emits failed-file records plus the final counts, omitting succeeded and skipped file records."
                );
                return Ok(());
            }
            value if value.starts_with('-') => return Err(format!("unknown option: {value}")),
            value => {
                if path.is_some() {
                    return Err("extract-audio-dir accepts one input directory".into());
                }
                path = Some(PathBuf::from(value));
            }
        }
    }
    if summary_only && !json {
        return Err("--summary can only be used with --json".to_owned());
    }
    let path = path.ok_or_else(|| "extract-audio-dir requires an input directory".to_owned())?;
    let output_root =
        output_root.ok_or_else(|| "extract-audio-dir requires --output-dir <DIR>".to_owned())?;
    if !matches!(format.as_str(), "mp3" | "wav" | "flac") {
        return Err("--format must be mp3, wav, or flac".to_owned());
    }
    if bitrate.is_some() && format != "mp3" {
        return Err("--bitrate can only be used with --format mp3".to_owned());
    }
    let bitrate = bitrate.unwrap_or_else(|| "192k".to_owned());
    const MP3_BITRATES: &[&str] = &["64k", "96k", "128k", "160k", "192k", "224k", "256k", "320k"];
    if format == "mp3" && !MP3_BITRATES.contains(&bitrate.as_str()) {
        return Err(format!(
            "unsupported MP3 bitrate '{bitrate}'; choose one of {}",
            MP3_BITRATES.join(", ")
        ));
    }
    let root = fs::canonicalize(&path)
        .map_err(|error| format!("cannot access {}: {error}", path.display()))?;
    if !is_real_directory(&root) {
        return Err(format!("expected a real directory: {}", root.display()));
    }
    let output_root = fs::canonicalize(&output_root).map_err(|error| {
        format!(
            "cannot access output directory {}: {error}",
            output_root.display()
        )
    })?;
    if !is_real_directory(&output_root) {
        return Err(format!(
            "expected a real output directory: {}",
            output_root.display()
        ));
    }
    if root == output_root {
        return Err("output directory must differ from the input directory".to_owned());
    }
    if root.starts_with(&output_root) {
        return Err("output directory must not contain the input directory".to_owned());
    }
    let output_inside_root = output_root.starts_with(&root);
    let ignored = ignored_by_git(&root)?;
    let mut records = Vec::<FilePathRecord>::new();
    let kinds = [Kind::Video, Kind::Audio];
    let filters = StrictWalkFilters {
        kinds: Some(&kinds),
        extensions: extensions.as_ref(),
        excluded_subtree: output_inside_root.then_some(output_root.as_path()),
        omit_extension: true,
        omit_modified_time: true,
        store_relative_path: true,
        ..StrictWalkFilters::default()
    };
    walk_strict_paths(
        &root,
        &root,
        include_hidden,
        SKIP_DIRS,
        &ignored,
        &filters,
        &mut records,
    )?;
    records.sort_unstable_by(|left, right| left.relative.cmp(&right.relative));
    if records.is_empty() {
        if json {
            let stdout = io::stdout();
            writeln!(
                stdout.lock(),
                "{{\"type\":\"summary\",\"succeeded\":0,\"skipped\":0,\"failed\":0}}"
            )
            .map_err(|error| format!("could not write audio summary JSON: {error}"))?;
        } else {
            println!("No media assets found.");
        }
        return Ok(());
    }

    let total = records.len();
    let mut plans: Vec<(Option<PathBuf>, Result<(), String>)> = records
        .iter()
        .map(|record| {
            let plan = (|| -> Result<PathBuf, String> {
                let relative = record.path.as_path();
                let stem = relative
                    .file_stem()
                    .and_then(|value| value.to_str())
                    .ok_or_else(|| {
                        "cannot create an audio output name from this path".to_owned()
                    })?;
                let mut relative_output = relative.to_path_buf();
                relative_output.set_file_name(format!("{stem}-audio.{format}"));
                let output = output_root.join(relative_output);
                Ok(output)
            })();
            match plan {
                Ok(output) => (Some(output), Ok(())),
                Err(error) => (None, Err(error)),
            }
        })
        .collect();
    mark_output_path_collisions(&mut plans, "audio");
    let next = std::sync::atomic::AtomicUsize::new(0);
    let completed = std::sync::atomic::AtomicUsize::new(0);
    let output_failed = std::sync::atomic::AtomicBool::new(false);
    let stream_error = std::sync::Mutex::new(None::<String>);
    let progress_output = std::sync::Mutex::new(());
    let succeeded_count = std::sync::atomic::AtomicUsize::new(0);
    let skipped_count = std::sync::atomic::AtomicUsize::new(0);
    let failed_count = std::sync::atomic::AtomicUsize::new(0);
    let outcomes = (!json).then(|| {
        (0..total)
            .map(|_| std::sync::atomic::AtomicU8::new(0))
            .collect::<Vec<_>>()
    });
    let errors = std::sync::Mutex::new(Vec::<(usize, String)>::new());
    std::thread::scope(|scope| {
        for _ in 0..jobs.min(total) {
            scope.spawn(|| {
                loop {
                    if output_failed.load(std::sync::atomic::Ordering::Acquire) {
                        break;
                    }
                    let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if index >= total {
                        break;
                    }
                    if output_failed.load(std::sync::atomic::Ordering::Acquire) {
                        break;
                    }
                    let record = &records[index];
                    let (output, plan) = &plans[index];
                    if !json {
                        println!("Extracting {}/{}: {}", index + 1, total, record.relative);
                    }
                    let result = plan.clone().and_then(|()| {
                        let output = output.as_ref().expect("successful plan has output");
                        let output_state = entry_state(output).map_err(|error| {
                            format!("cannot inspect output {}: {error}", output.display())
                        })?;
                        if output_state == EntryState::Other {
                            return Err(format!(
                                "output path exists but is not a regular file: {}",
                                output.display()
                            ));
                        }
                        if skip_existing && output_state == EntryState::File {
                            return Ok(false);
                        }
                        if let Some(parent) = output.parent() {
                            fs::create_dir_all(parent).map_err(|error| {
                                format!(
                                    "could not create audio directory {}: {error}",
                                    parent.display()
                                )
                            })?;
                        }
                        run_extract_audio_with_options(
                            root.join(&record.path),
                            output.clone(),
                            &format,
                            &bitrate,
                            stream,
                            true,
                        )
                        .map(|()| true)
                    });
                    if json {
                        let _guard = progress_output
                            .lock()
                            .expect("audio progress output lock poisoned");
                        let current =
                            completed.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                        if (!summary_only || result.is_err())
                            && let Err(error) = print_batch_result_json(
                                current,
                                total,
                                &record.relative,
                                output.as_deref(),
                                &result,
                            )
                        {
                            output_failed.store(true, std::sync::atomic::Ordering::Release);
                            stream_error
                                .lock()
                                .expect("audio output error lock poisoned")
                                .get_or_insert(error);
                            break;
                        }
                        match &result {
                            Ok(true) => {
                                succeeded_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            }
                            Ok(false) => {
                                skipped_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            }
                            Err(_) => {
                                failed_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            }
                        }
                    } else {
                        match result {
                            Ok(true) => outcomes.as_ref().expect("human output outcomes allocated")
                                [index]
                                .store(1, std::sync::atomic::Ordering::Relaxed),
                            Ok(false) => {
                                outcomes.as_ref().expect("human output outcomes allocated")[index]
                                    .store(2, std::sync::atomic::Ordering::Relaxed)
                            }
                            Err(error) => {
                                outcomes.as_ref().expect("human output outcomes allocated")[index]
                                    .store(3, std::sync::atomic::Ordering::Relaxed);
                                errors
                                    .lock()
                                    .expect("audio error lock poisoned")
                                    .push((index, error));
                            }
                        }
                    }
                }
            });
        }
    });
    if let Some(error) = stream_error
        .into_inner()
        .map_err(|_| "audio output error lock poisoned".to_owned())?
    {
        return Err(error);
    }
    let mut errors = errors
        .into_inner()
        .map_err(|_| "audio error lock poisoned".to_owned())?;
    let (succeeded, skipped, failed) = if json {
        (
            succeeded_count.load(std::sync::atomic::Ordering::Relaxed),
            skipped_count.load(std::sync::atomic::Ordering::Relaxed),
            failed_count.load(std::sync::atomic::Ordering::Relaxed),
        )
    } else {
        errors.sort_unstable_by_key(|(index, _)| *index);
        let mut errors = errors.into_iter().peekable();
        let mut succeeded = 0_usize;
        let mut skipped = 0_usize;
        let mut failed = 0_usize;
        for (index, outcome) in outcomes
            .as_ref()
            .expect("human output outcomes allocated")
            .iter()
            .enumerate()
        {
            match outcome.load(std::sync::atomic::Ordering::Relaxed) {
                1 => {
                    succeeded += 1;
                    if let Some(output) = &plans[index].0 {
                        println!("Created audio {}", output.display());
                    }
                }
                2 => {
                    skipped += 1;
                    if let Some(output) = &plans[index].0 {
                        println!("Skipped existing audio {}", output.display());
                    }
                }
                3 => {
                    failed += 1;
                    let (error_index, error) = errors
                        .next()
                        .expect("failed audio outcome has a recorded error");
                    debug_assert_eq!(error_index, index);
                    eprintln!("mediaforge: {}: {error}", records[index].relative);
                }
                _ => unreachable!("audio outcome is recorded before workers finish"),
            }
        }
        (succeeded, skipped, failed)
    };
    if json {
        let stdout = io::stdout();
        writeln!(
            stdout.lock(),
            "{{\"type\":\"summary\",\"succeeded\":{succeeded},\"skipped\":{skipped},\"failed\":{failed}}}"
        )
        .map_err(|error| format!("could not write audio summary JSON: {error}"))?;
    } else {
        println!(
            "Audio extraction finished: {succeeded} succeeded, {skipped} skipped, {failed} failed."
        );
    }
    if failed > 0 {
        return Err(format!("audio extraction failed for {failed} file(s)"));
    }
    Ok(())
}

fn directory_size(root: &Path) -> Result<(u64, u64), String> {
    if !is_real_directory(root) {
        return Err(format!("expected a real directory: {}", root.display()));
    }
    let mut pending = vec![root.to_path_buf()];
    let mut bytes = 0_u64;
    let mut files = 0_u64;
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory)
            .map_err(|error| format!("cannot read {}: {error}", directory.display()))?
        {
            let entry = entry.map_err(|error| {
                format!("cannot read entry in {}: {error}", directory.display())
            })?;
            let file_type = entry
                .file_type()
                .map_err(|error| format!("cannot inspect {}: {error}", entry.path().display()))?;
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                pending.push(entry.path());
            } else if file_type.is_file() {
                let length = entry
                    .metadata()
                    .map_err(|error| format!("cannot inspect {}: {error}", entry.path().display()))?
                    .len();
                bytes = bytes.saturating_add(length);
                files = files.saturating_add(1);
            }
        }
    }
    Ok((bytes, files))
}

static PROJECT_FORMAT_BY_EXTENSION: LazyLock<Vec<(&'static str, &'static str)>> =
    LazyLock::new(|| {
        let mut formats = vec![
            ("blend", "Blender"),
            ("drp", "DaVinci Resolve"),
            ("dra", "DaVinci Resolve Archive"),
            ("prproj", "Adobe Premiere Pro"),
            ("aep", "Adobe After Effects"),
            ("aepx", "Adobe After Effects XML"),
            ("aaf", "Advanced Authoring Format"),
            ("edl", "Edit Decision List"),
            ("mlt", "MLT project"),
            ("fcpxml", "Final Cut Pro XML"),
            ("wproj", "Wwise Project"),
            ("wwu", "Wwise Work Unit"),
            ("fspro", "FMOD Studio Project"),
        ];
        formats.sort_unstable_by_key(|(extension, _)| *extension);
        formats
    });

fn project_file_kind(path: &str) -> Option<&'static str> {
    let extension = Path::new(path).extension()?.to_str()?;
    PROJECT_FORMAT_BY_EXTENSION
        .binary_search_by(|(candidate, _)| {
            compare_extensions_case_insensitive(candidate, extension)
        })
        .ok()
        .map(|index| PROJECT_FORMAT_BY_EXTENSION[index].1)
}

fn json_top_level_string_field(contents: &str, field: &str) -> Option<String> {
    let bytes = contents.as_bytes();
    let mut index = if contents.starts_with('\u{feff}') {
        3
    } else {
        0
    };
    while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
        index += 1;
    }
    if bytes.get(index) != Some(&b'{') {
        return None;
    }

    let mut object_depth = 0_usize;
    let mut array_depth = 0_usize;
    while index < bytes.len() {
        match bytes[index] {
            b'{' => {
                object_depth += 1;
                index += 1;
            }
            b'}' => {
                object_depth = object_depth.saturating_sub(1);
                index += 1;
            }
            b'[' => {
                array_depth += 1;
                index += 1;
            }
            b']' => {
                array_depth = array_depth.saturating_sub(1);
                index += 1;
            }
            b'"' => {
                let start = index + 1;
                index = start;
                let mut escaped = false;
                while index < bytes.len() {
                    match bytes[index] {
                        b'\\' if !escaped => escaped = true,
                        b'"' if !escaped => break,
                        _ => escaped = false,
                    }
                    index += 1;
                }
                if index == bytes.len() {
                    return None;
                }
                let token = &contents[start..index];
                let is_top_level_key = object_depth == 1
                    && array_depth == 0
                    && !token.contains('\\')
                    && token == field;
                index += 1;
                if is_top_level_key {
                    while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
                        index += 1;
                    }
                    if bytes.get(index) != Some(&b':') {
                        continue;
                    }
                    index += 1;
                    while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
                        index += 1;
                    }
                    if bytes.get(index) != Some(&b'"') {
                        continue;
                    }
                    let value_start = index + 1;
                    index = value_start;
                    let mut escaped = false;
                    while index < bytes.len() {
                        match bytes[index] {
                            b'\\' if !escaped => escaped = true,
                            b'"' if !escaped => break,
                            _ => escaped = false,
                        }
                        index += 1;
                    }
                    if index == bytes.len() {
                        return None;
                    }
                    let value = &contents[value_start..index];
                    return (!value.contains('\\')).then(|| value.to_owned());
                }
            }
            _ => index += 1,
        }
    }
    None
}

fn read_godot_hex_escape(chars: &mut std::str::Chars<'_>, digits: usize) -> Option<u32> {
    let mut value = 0_u32;
    for _ in 0..digits {
        value = value
            .checked_mul(16)?
            .checked_add(chars.next()?.to_digit(16)?)?;
    }
    Some(value)
}

fn parse_godot_string_array(value: &str) -> Vec<String> {
    let value = value.trim();
    let array = value
        .strip_prefix("PackedStringArray(")
        .or_else(|| value.strip_prefix("PoolStringArray("));
    let Some(array) = array else {
        return Vec::new();
    };
    let mut chars = array.chars();
    let mut values = Vec::new();
    let mut closed_array = false;
    while let Some(character) = chars.next() {
        match character {
            ')' => {
                closed_array = true;
                break;
            }
            ',' | ' ' | '\t' | '\r' | '\n' => {}
            '"' => {
                let mut value = String::new();
                let mut closed = false;
                while let Some(character) = chars.next() {
                    match character {
                        '"' => {
                            closed = true;
                            break;
                        }
                        '\\' => match chars.next() {
                            Some('n') => value.push('\n'),
                            Some('r') => value.push('\r'),
                            Some('t') => value.push('\t'),
                            Some('b') => value.push('\u{0008}'),
                            Some('f') => value.push('\u{000c}'),
                            Some('"') => value.push('"'),
                            Some('\\') => value.push('\\'),
                            Some('u') => {
                                let Some(codepoint) = read_godot_hex_escape(&mut chars, 4) else {
                                    return Vec::new();
                                };
                                let codepoint = if (0xd800..=0xdbff).contains(&codepoint) {
                                    if chars.next() != Some('\\') || chars.next() != Some('u') {
                                        return Vec::new();
                                    }
                                    let Some(low) = read_godot_hex_escape(&mut chars, 4) else {
                                        return Vec::new();
                                    };
                                    if !(0xdc00..=0xdfff).contains(&low) {
                                        return Vec::new();
                                    }
                                    0x10000 + ((codepoint - 0xd800) << 10) + (low - 0xdc00)
                                } else {
                                    codepoint
                                };
                                let Some(character) = char::from_u32(codepoint) else {
                                    return Vec::new();
                                };
                                value.push(character);
                            }
                            Some(other) => {
                                value.push('\\');
                                value.push(other);
                            }
                            None => return Vec::new(),
                        },
                        other => value.push(other),
                    }
                }
                if !closed {
                    return Vec::new();
                }
                values.push(value);
            }
            _ => return Vec::new(),
        }
    }
    if !closed_array || chars.any(|character| !character.is_whitespace()) {
        return Vec::new();
    }
    values
}

fn godot_project_features(contents: &str) -> Vec<String> {
    let mut section = "";
    for (index, raw_line) in contents.lines().enumerate() {
        let line = raw_line.trim();
        let line = if index == 0 {
            line.trim_start_matches('\u{feff}').trim_start()
        } else {
            line
        };
        if line.starts_with('[') && line.ends_with(']') {
            section = &line[1..line.len() - 1];
            continue;
        }
        if section == "application"
            && let Some((key, value)) = line.split_once('=')
            && key.trim() == "config/features"
        {
            return parse_godot_string_array(value.trim());
        }
    }
    Vec::new()
}

fn run_project(args: &[String]) -> Result<(), String> {
    let mut json = false;
    let mut compact = false;
    let mut cache_sizes = true;
    let mut editor_files = true;
    let mut path = None;
    for arg in args {
        match arg.as_str() {
            "--json" => json = true,
            "--compact" => compact = true,
            "--no-cache-sizes" => cache_sizes = false,
            "--no-editor-files" => editor_files = false,
            "--help" | "-h" => {
                println!(
                    "Usage: mediaforge project [PATH] [--json] [--compact] [--no-cache-sizes] [--no-editor-files]\n\nInspect Unity, Unreal, Godot, Blender and video-editor project markers and report generated cache sizes. --compact removes formatting whitespace from JSON output and requires --json. --no-cache-sizes skips recursive cache folder measurement. --no-editor-files also skips the recursive search for editor project files, leaving a fast project-marker inspection. This command is read-only."
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
    if compact && !json {
        return Err("--compact can only be used with --json".to_owned());
    }
    let path = path.unwrap_or_else(|| PathBuf::from("."));
    let root = fs::canonicalize(&path)
        .map_err(|error| format!("cannot access {}: {error}", path.display()))?;
    if !root.is_dir() {
        return Err(format!("expected a project directory: {}", root.display()));
    }

    let mut root_entries = Vec::new();
    let mut unreal_files = Vec::<(String, Option<String>)>::new();
    let entries = fs::read_dir(&root)
        .map_err(|error| format!("cannot read project directory {}: {error}", root.display()))?;
    for entry in entries {
        let entry =
            entry.map_err(|error| format!("cannot read entry in {}: {error}", root.display()))?;
        let file_type = entry.file_type().map_err(|error| {
            format!(
                "cannot inspect project entry {}: {error}",
                entry.path().display()
            )
        })?;
        let entry_path = entry.path();
        if file_type.is_file()
            && entry_path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("uproject"))
        {
            let project_file = entry.file_name().to_string_lossy().into_owned();
            let contents = fs::read_to_string(&entry_path).map_err(|error| {
                format!(
                    "cannot read Unreal project descriptor {}: {error}",
                    entry_path.display()
                )
            })?;
            let association = json_top_level_string_field(&contents, "EngineAssociation")
                .filter(|association| !association.trim().is_empty());
            unreal_files.push((project_file, association));
        }
        if editor_files {
            root_entries.push((entry, file_type));
        }
    }
    let has_unity_assets = is_real_directory(&root.join("Assets"));
    let has_godot_project = is_real_file(&root.join("project.godot"));
    let mut engines = Vec::<(&str, Vec<&str>, Vec<&str>)>::new();
    if has_unity_assets || is_real_file(&root.join("ProjectSettings/ProjectVersion.txt")) {
        engines.push((
            "Unity",
            vec!["Assets", "ProjectSettings"],
            vec!["Library", "Temp", "Obj"],
        ));
    }
    let unity_version_path = root.join("ProjectSettings/ProjectVersion.txt");
    let unity_version_contents = match is_real_file(&unity_version_path)
        .then(|| fs::read_to_string(&unity_version_path))
        .transpose()
    {
        Ok(contents) => contents,
        Err(error) => {
            return Err(format!(
                "cannot read Unity version file {}: {error}",
                unity_version_path.display()
            ));
        }
    };
    let unity_version = unity_version_contents.and_then(|contents| {
        contents.lines().find_map(|line| {
            line.trim()
                .strip_prefix("m_EditorVersion:")
                .map(str::trim)
                .filter(|version| !version.is_empty())
                .map(str::to_owned)
        })
    });
    unreal_files.sort_unstable_by(|left, right| compare_paths_case_insensitive(&left.0, &right.0));
    if !unreal_files.is_empty() {
        engines.push((
            "Unreal Engine",
            vec!["Content"],
            vec!["Intermediate", "Saved", "DerivedDataCache", "Binaries"],
        ));
    }
    let godot_project_path = root.join("project.godot");
    let godot_features = if has_godot_project {
        let contents = fs::read_to_string(&godot_project_path).map_err(|error| {
            format!(
                "cannot read Godot project file {}: {error}",
                godot_project_path.display()
            )
        })?;
        let features = godot_project_features(&contents);
        engines.push(("Godot", vec!["project.godot"], vec![".godot", ".import"]));
        features
    } else {
        Vec::new()
    };

    let mut project_files = Vec::<(String, &'static str)>::new();
    if editor_files {
        let ignored = ignored_by_git(&root)?;
        let mut pending = vec![(
            root.clone(),
            Arc::<[IgnoreRule]>::from(ignored),
            Some(root_entries),
        )];
        while let Some((directory, active_ignored, preloaded_entries)) = pending.pop() {
            let mut process_entry = |entry: fs::DirEntry,
                                     file_type: fs::FileType|
             -> Result<(), String> {
                let path = entry.path();
                if file_type.is_symlink() {
                    return Ok(());
                }
                if file_type.is_dir() {
                    let name = entry.file_name();
                    let lossy_name = name.to_string_lossy();
                    if directory_name_matches(&lossy_name, SKIP_DIRS)
                        || directory_name_matches(&lossy_name, PROJECT_ARTIFACT_DIRS)
                    {
                        return Ok(());
                    }
                    if !active_ignored.is_empty() {
                        let relative = path
                            .strip_prefix(&root)
                            .unwrap_or(&path)
                            .to_string_lossy()
                            .replace('\\', "/");
                        if is_ignored_directory(&relative, &active_ignored) {
                            return Ok(());
                        }
                    }
                    let child_rules = extend_ignore_rules(&root, &path, active_ignored.as_ref())?;
                    let child_ignored = match child_rules {
                        Cow::Borrowed(_) => Arc::clone(&active_ignored),
                        Cow::Owned(rules) => Arc::from(rules),
                    };
                    pending.push((path, child_ignored, None));
                } else if file_type.is_file() {
                    let relative = path
                        .strip_prefix(&root)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .replace('\\', "/");
                    if is_ignored(&relative, &active_ignored) {
                        return Ok(());
                    }
                    if let Some(kind) = project_file_kind(&relative) {
                        project_files.push((relative, kind));
                    }
                }
                Ok(())
            };
            if let Some(entries) = preloaded_entries {
                for (entry, file_type) in entries {
                    process_entry(entry, file_type)?;
                }
            } else {
                let entries = fs::read_dir(&directory).map_err(|error| {
                    format!(
                        "cannot read project directory {}: {error}",
                        directory.display()
                    )
                })?;
                for entry in entries {
                    let entry = entry.map_err(|error| {
                        format!("cannot read entry in {}: {error}", directory.display())
                    })?;
                    let file_type = entry.file_type().map_err(|error| {
                        format!(
                            "cannot inspect project entry {}: {error}",
                            entry.path().display()
                        )
                    })?;
                    process_entry(entry, file_type)?;
                }
            }
        }
    }
    project_files.sort_unstable_by(|left, right| compare_paths_case_insensitive(&left.0, &right.0));

    let mut missing = Vec::<(&str, &str)>::new();
    let mut caches = Vec::<(String, u64, u64)>::new();
    for (engine, required, cache_names) in &engines {
        for required_path in required {
            let required_entry = root.join(required_path);
            let present = match *required_path {
                "project.godot" => has_godot_project,
                "Assets" => has_unity_assets,
                _ => is_real_directory(&required_entry),
            };
            if !present {
                missing.push((engine, required_path));
            }
        }
        for cache_name in cache_names {
            if cache_sizes {
                let cache_path = root.join(cache_name);
                if is_real_directory(&cache_path) {
                    let (bytes, files) = directory_size(&cache_path)?;
                    caches.push(((*cache_name).to_owned(), bytes, files));
                }
            }
        }
    }

    if json {
        let stdout = io::stdout();
        let output = io::BufWriter::with_capacity(64 * 1024, stdout.lock());
        let mut writer = JsonWhitespaceWriter::new(output, compact);
        let result = (|| -> io::Result<()> {
            writer.write_all(
                b"{\n  \"schema_version\": 1,\n  \"type\": \"project\",\n  \"root\": ",
            )?;
            write_json_string(&mut writer, &root.to_string_lossy())?;
            writer.write_all(b",\n  \"engines\": [")?;
            for (index, (name, _, _)) in engines.iter().enumerate() {
                if index > 0 {
                    writer.write_all(b", ")?;
                }
                write_json_string(&mut writer, name)?;
            }
            writer.write_all(b"],\n  \"unity_version\": ")?;
            if let Some(version) = unity_version.as_deref() {
                write_json_string(&mut writer, version)?;
            } else {
                writer.write_all(b"null")?;
            }
            writer.write_all(b",\n  \"unreal_project_files\": [")?;
            for (index, (file, _)) in unreal_files.iter().enumerate() {
                if index > 0 {
                    writer.write_all(b", ")?;
                }
                write_json_string(&mut writer, file)?;
            }
            writer.write_all(b"],\n  \"unreal_engine_associations\": [\n")?;
            for (index, (file, association)) in unreal_files.iter().enumerate() {
                if index > 0 {
                    writer.write_all(b",\n")?;
                }
                writer.write_all(b"    {\"project\": ")?;
                write_json_string(&mut writer, file)?;
                writer.write_all(b", \"engine_association\": ")?;
                if let Some(association) = association.as_deref() {
                    write_json_string(&mut writer, association)?;
                } else {
                    writer.write_all(b"null")?;
                }
                writer.write_all(b"}")?;
            }
            writer.write_all(b"\n  ],\n  \"godot_features\": [")?;
            for (index, feature) in godot_features.iter().enumerate() {
                if index > 0 {
                    writer.write_all(b", ")?;
                }
                write_json_string(&mut writer, feature)?;
            }
            writer.write_all(b"],\n  \"missing_required_paths\": [\n")?;
            for (index, (engine, missing_path)) in missing.iter().enumerate() {
                if index > 0 {
                    writer.write_all(b",\n")?;
                }
                writer.write_all(b"    {\"engine\": ")?;
                write_json_string(&mut writer, engine)?;
                writer.write_all(b", \"path\": ")?;
                write_json_string(&mut writer, missing_path)?;
                writer.write_all(b"}")?;
            }
            writer.write_all(b"\n  ],\n  \"editor_project_files_included\": ")?;
            writer.write_all(if editor_files { b"true" } else { b"false" })?;
            writer.write_all(b",\n  \"cache_sizes_included\": ")?;
            writer.write_all(if cache_sizes { b"true" } else { b"false" })?;
            writer.write_all(b",\n  \"known_build_or_cache_dirs\": [\n")?;
            for (index, (name, bytes, files)) in caches.iter().enumerate() {
                if index > 0 {
                    writer.write_all(b",\n")?;
                }
                writer.write_all(b"    {\"path\": ")?;
                write_json_string(&mut writer, name)?;
                write!(
                    writer,
                    ", \"files\": {files}, \"bytes\": {bytes}, \"size\": "
                )?;
                write_json_string(&mut writer, &human_size(*bytes))?;
                writer.write_all(b"}")?;
            }
            writer.write_all(b"\n  ],\n  \"editor_project_files\": [\n")?;
            for (index, (relative, kind)) in project_files.iter().enumerate() {
                if index > 0 {
                    writer.write_all(b",\n")?;
                }
                writer.write_all(b"    {\"path\": ")?;
                write_json_string(&mut writer, relative)?;
                writer.write_all(b", \"kind\": ")?;
                write_json_string(&mut writer, kind)?;
                writer.write_all(b"}")?;
            }
            writer.write_all(b"\n  ]\n}\n")?;
            writer.flush()
        })();
        result.map_err(|error| format!("could not write project JSON: {error}"))?;
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
        for (project_file, association) in unreal_files {
            if let Some(association) = association {
                println!("Unreal project: {project_file} (EngineAssociation: {association})");
            } else {
                println!("Unreal project: {project_file}");
            }
        }
        if !godot_features.is_empty() {
            println!("Godot project features: {}", godot_features.join(", "));
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
        if !cache_sizes {
            println!("\nCache size measurement skipped.");
        }
        if !editor_files {
            println!("\nEditor project file search skipped.");
        } else if !project_files.is_empty() {
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

fn unity_meta_sidecar(path: &Path) -> PathBuf {
    let mut file_name = path.file_name().unwrap_or_default().to_os_string();
    file_name.push(".meta");
    path.with_file_name(file_name)
}

fn unity_meta_sidecar_exists(path: &Path) -> bool {
    is_real_file(&unity_meta_sidecar(path))
}

fn unity_meta_sidecar_exists_in_directory(
    path: &Path,
    sibling_sidecars_lowercase: &HashSet<String>,
) -> bool {
    let asset_name = path.file_name().unwrap_or_default().to_string_lossy();
    if sibling_sidecars_lowercase.contains(&asset_name.to_ascii_lowercase()) {
        return true;
    }
    unity_meta_sidecar_exists(path)
}

fn unity_meta_target(path: &Path) -> Option<PathBuf> {
    let file_name = path.file_name()?.to_string_lossy();
    let target_name = strip_ascii_case_insensitive_suffix(&file_name, ".meta")?;
    (!target_name.is_empty()).then(|| path.with_file_name(target_name))
}

fn unity_meta_target_exists_in_directory(
    target: &Path,
    sibling_entries: &HashSet<std::ffi::OsString>,
) -> bool {
    if target
        .file_name()
        .is_some_and(|name| sibling_entries.contains(name))
    {
        return true;
    }
    is_real_file(target) || is_real_directory(target)
}

fn unity_hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn unity_meta_guid(mut reader: impl BufRead) -> io::Result<Result<Option<[u8; 16]>, ()>> {
    let mut guid = None;
    let mut line = String::new();
    let mut first_line = true;
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let raw_line = line.trim_end_matches(['\r', '\n']);
        let line = if first_line {
            first_line = false;
            raw_line.trim_start_matches('\u{feff}')
        } else {
            raw_line
        };
        if line.trim_start() != line {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if key.trim() != "guid" {
            continue;
        }
        if guid.is_some() {
            return Ok(Err(()));
        }
        let value = value.trim();
        let value = match (value.starts_with('"'), value.ends_with('"')) {
            (true, true) if value.len() >= 2 => &value[1..value.len() - 1],
            (false, false) => value,
            _ => return Ok(Err(())),
        };
        if value.len() != 32 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Ok(Err(()));
        }
        let mut parsed = [0_u8; 16];
        let bytes = value.as_bytes();
        for index in 0..16 {
            let Some(high) = unity_hex_nibble(bytes[index * 2]) else {
                return Ok(Err(()));
            };
            let Some(low) = unity_hex_nibble(bytes[index * 2 + 1]) else {
                return Ok(Err(()));
            };
            parsed[index] = (high << 4) | low;
        }
        guid = Some(parsed);
    }
    Ok(Ok(guid))
}

fn read_unity_meta_guid(path: &Path) -> Result<Result<Option<[u8; 16]>, ()>, String> {
    let file = File::open(path)
        .map_err(|error| format!("cannot read Unity metadata {}: {error}", path.display()))?;
    unity_meta_guid(BufReader::new(file))
        .map_err(|error| format!("cannot read Unity metadata {}: {error}", path.display()))
}

fn unity_guid_string(guid: [u8; 16]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(32);
    for byte in guid {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

fn run_unity_meta(args: &[String]) -> Result<(), String> {
    let mut json = false;
    let mut compact = false;
    let mut summary = false;
    let mut sidecars_only = false;
    let mut include_hidden = false;
    let mut path = None;
    for arg in args {
        match arg.as_str() {
            "--json" => json = true,
            "--compact" => compact = true,
            "--summary" => summary = true,
            "--sidecars-only" => sidecars_only = true,
            "--include-hidden" => include_hidden = true,
            "--help" | "-h" => {
                println!(
                    "Usage: mediaforge unity-meta [PROJECT_PATH] [--json] [--compact] [--summary] [--sidecars-only] [--include-hidden]\n\nCheck Unity Assets for missing or orphaned .meta files, missing or malformed GUIDs, and duplicate GUID collisions. --compact removes formatting whitespace from JSON output and requires --json. --summary verifies all assets but omits issue path lists and duplicate GUID details. --sidecars-only checks missing/orphaned .meta files without opening sidecars to inspect GUIDs. Honors .gitignore by default, follows no symlinks, and never changes files. Exits with an error status when checked issues are found."
                );
                return Ok(());
            }
            value if value.starts_with('-') => return Err(format!("unknown option: {value}")),
            value => {
                if path.is_some() {
                    return Err("unity-meta accepts one Unity project path".to_owned());
                }
                path = Some(PathBuf::from(value));
            }
        }
    }
    if compact && !json {
        return Err("--compact can only be used with --json".to_owned());
    }
    let path = path.unwrap_or_else(|| PathBuf::from("."));
    let root = fs::canonicalize(&path)
        .map_err(|error| format!("cannot access {}: {error}", path.display()))?;
    if !root.is_dir() {
        return Err(format!("expected a project directory: {}", root.display()));
    }
    let assets = root.join("Assets");
    if !is_real_directory(&assets) {
        return Err(format!(
            "Unity Assets directory was not found in {}",
            root.display()
        ));
    }

    let rules = ignored_by_git(&root)?;
    let mut pending = vec![(assets.clone(), Arc::<[IgnoreRule]>::from(rules))];
    let mut checked = 1_usize;
    let mut missing_meta_count = 0_usize;
    let mut orphan_meta_count = 0_usize;
    let mut missing_guid_count = 0_usize;
    let mut invalid_guid_count = 0_usize;
    let mut summary_duplicate_guid_count = 0_usize;
    let mut missing_meta = Vec::<String>::new();
    let mut orphan_meta = Vec::<String>::new();
    let mut missing_guid = Vec::<String>::new();
    let mut invalid_guid = Vec::<String>::new();
    let mut guid_paths = HashMap::<[u8; 16], (String, Option<Vec<String>>)>::new();
    let mut summary_guids = HashSet::<[u8; 16]>::new();
    let assets_meta = unity_meta_sidecar(&assets);
    if !is_real_file(&assets_meta) {
        missing_meta_count += 1;
        if !summary {
            missing_meta.push("Assets".to_owned());
        }
    } else {
        checked = checked.saturating_add(1);
        if !sidecars_only {
            match read_unity_meta_guid(&assets_meta)? {
                Ok(Some(guid)) => {
                    if summary {
                        summary_guids.insert(guid);
                    } else {
                        guid_paths.insert(guid, ("Assets.meta".to_owned(), None));
                    }
                }
                Ok(None) => {
                    missing_guid_count += 1;
                    if !summary {
                        missing_guid.push("Assets.meta".to_owned());
                    }
                }
                Err(()) => {
                    invalid_guid_count += 1;
                    if !summary {
                        invalid_guid.push("Assets.meta".to_owned());
                    }
                }
            }
        }
    }
    while let Some((directory, active_rules)) = pending.pop() {
        let entries = fs::read_dir(&directory)
            .map_err(|error| {
                format!(
                    "cannot read Unity asset directory {}: {error}",
                    directory.display()
                )
            })?
            .map(|entry| {
                entry.map_err(|error| {
                    format!("cannot read entry in {}: {error}", directory.display())
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        // Each output list is sorted after traversal, so per-directory sorting is redundant.
        let mut sibling_entries = HashSet::with_capacity(entries.len());
        let mut sibling_sidecars_lowercase = HashSet::with_capacity(entries.len() / 2);
        let mut entry_types = Vec::with_capacity(entries.len());
        for entry in &entries {
            let file_type = entry.file_type().map_err(|error| {
                format!(
                    "cannot inspect Unity asset {}: {error}",
                    entry.path().display()
                )
            })?;
            if file_type.is_file() || file_type.is_dir() {
                sibling_entries.insert(entry.file_name());
            }
            if file_type.is_file() {
                let file_name = entry.file_name();
                let file_name = file_name.to_string_lossy();
                if let Some(asset_name) = strip_ascii_case_insensitive_suffix(&file_name, ".meta") {
                    sibling_sidecars_lowercase.insert(asset_name.to_ascii_lowercase());
                }
            }
            entry_types.push(file_type);
        }
        for (entry, file_type) in entries.into_iter().zip(entry_types) {
            let entry_path = entry.path();
            if file_type.is_symlink() || (!include_hidden && is_dot_hidden(&entry_path)) {
                continue;
            }
            let meta_target = file_type
                .is_file()
                .then(|| unity_meta_target(&entry_path))
                .flatten();
            if file_type.is_file() && active_rules.is_empty() && meta_target.is_none() {
                if !include_hidden && has_hidden_attribute(&entry_path) {
                    continue;
                }
                checked = checked.saturating_add(1);
                if !unity_meta_sidecar_exists_in_directory(&entry_path, &sibling_sidecars_lowercase)
                {
                    missing_meta_count += 1;
                    if !summary {
                        missing_meta.push(relative_path_text(&root, &entry_path));
                    }
                }
                continue;
            }
            let relative = entry_path
                .strip_prefix(&root)
                .unwrap_or(&entry_path)
                .to_string_lossy()
                .replace('\\', "/");
            if file_type.is_dir() {
                if is_ignored_directory(&relative, &active_rules)
                    || (!include_hidden && has_hidden_attribute(&entry_path))
                {
                    continue;
                }
                checked = checked.saturating_add(1);
                if !unity_meta_sidecar_exists_in_directory(&entry_path, &sibling_sidecars_lowercase)
                {
                    missing_meta_count += 1;
                    if !summary {
                        missing_meta.push(relative);
                    }
                }
                let child_rules = extend_ignore_rules(&root, &entry_path, &active_rules)?;
                let child_rules = match child_rules {
                    Cow::Borrowed(_) => Arc::clone(&active_rules),
                    Cow::Owned(rules) => Arc::from(rules),
                };
                pending.push((entry_path, child_rules));
            } else if file_type.is_file() {
                if is_ignored(&relative, &active_rules)
                    || (!include_hidden && has_hidden_attribute(&entry_path))
                {
                    continue;
                }
                if let Some(target) = meta_target {
                    checked = checked.saturating_add(1);
                    if !unity_meta_target_exists_in_directory(&target, &sibling_entries) {
                        orphan_meta_count += 1;
                        if !summary {
                            orphan_meta.push(relative);
                        }
                    } else if !sidecars_only {
                        match read_unity_meta_guid(&entry_path)? {
                            Ok(Some(guid)) => {
                                if summary {
                                    if !summary_guids.insert(guid) {
                                        summary_duplicate_guid_count += 1;
                                    }
                                } else {
                                    match guid_paths.entry(guid) {
                                        std::collections::hash_map::Entry::Vacant(entry) => {
                                            entry.insert((relative, None));
                                        }
                                        std::collections::hash_map::Entry::Occupied(mut entry) => {
                                            let (first, duplicates) = entry.get_mut();
                                            let paths = duplicates
                                                .get_or_insert_with(|| vec![std::mem::take(first)]);
                                            paths.push(relative);
                                        }
                                    }
                                }
                            }
                            Ok(None) => {
                                missing_guid_count += 1;
                                if !summary {
                                    missing_guid.push(relative);
                                }
                            }
                            Err(()) => {
                                invalid_guid_count += 1;
                                if !summary {
                                    invalid_guid.push(relative);
                                }
                            }
                        }
                    }
                } else {
                    checked = checked.saturating_add(1);
                    if !unity_meta_sidecar_exists_in_directory(
                        &entry_path,
                        &sibling_sidecars_lowercase,
                    ) {
                        missing_meta_count += 1;
                        if !summary {
                            missing_meta.push(relative);
                        }
                    }
                }
            }
        }
    }
    missing_meta.sort_unstable();
    orphan_meta.sort_unstable();
    missing_guid.sort_unstable();
    invalid_guid.sort_unstable();
    let mut duplicate_guids = guid_paths
        .into_iter()
        .filter_map(|(guid, (_, paths))| paths.map(|paths| (guid, paths)))
        .collect::<Vec<_>>();
    duplicate_guids.sort_unstable_by_key(|entry| entry.0);
    for (_, paths) in &mut duplicate_guids {
        paths.sort_unstable();
    }
    let duplicate_guid_count = if summary {
        summary_duplicate_guid_count
    } else {
        duplicate_guids
            .iter()
            .map(|(_, paths)| paths.len() - 1)
            .sum::<usize>()
    };
    let issue_count = missing_meta_count
        + orphan_meta_count
        + missing_guid_count
        + invalid_guid_count
        + duplicate_guid_count;
    if json {
        let stdout = io::stdout();
        let output_buffer = io::BufWriter::with_capacity(64 * 1024, stdout.lock());
        let mut output = JsonWhitespaceWriter::new(output_buffer, compact);
        let result = (|| -> io::Result<()> {
            output.write_all(b"{\"schema_version\":1,\"type\":\"unity_meta\",\"root\":")?;
            write_json_string(&mut output, &root.to_string_lossy())?;
            write!(
                output,
                ",\"checked\":{checked},\"details_included\":{},\"guid_checks_included\":{},\"missing_meta_count\":{},\"orphan_meta_count\":{},\"missing_guid_count\":{},\"invalid_guid_count\":{},\"duplicate_guid_count\":{duplicate_guid_count},\"missing_meta\":[",
                !summary,
                !sidecars_only,
                missing_meta_count,
                orphan_meta_count,
                missing_guid_count,
                invalid_guid_count
            )?;
            for (index, path) in missing_meta
                .iter()
                .take(if summary { 0 } else { usize::MAX })
                .enumerate()
            {
                if index > 0 {
                    output.write_all(b",")?;
                }
                write_json_string(&mut output, path)?;
            }
            output.write_all(b"],\"orphan_meta\":[")?;
            for (index, path) in orphan_meta
                .iter()
                .take(if summary { 0 } else { usize::MAX })
                .enumerate()
            {
                if index > 0 {
                    output.write_all(b",")?;
                }
                write_json_string(&mut output, path)?;
            }
            output.write_all(b"],\"missing_guid\":[")?;
            for (index, path) in missing_guid
                .iter()
                .take(if summary { 0 } else { usize::MAX })
                .enumerate()
            {
                if index > 0 {
                    output.write_all(b",")?;
                }
                write_json_string(&mut output, path)?;
            }
            output.write_all(b"],\"invalid_guid\":[")?;
            for (index, path) in invalid_guid
                .iter()
                .take(if summary { 0 } else { usize::MAX })
                .enumerate()
            {
                if index > 0 {
                    output.write_all(b",")?;
                }
                write_json_string(&mut output, path)?;
            }
            output.write_all(b"],\"duplicate_guids\":[")?;
            for (index, (guid, paths)) in duplicate_guids
                .iter()
                .take(if summary { 0 } else { usize::MAX })
                .enumerate()
            {
                if index > 0 {
                    output.write_all(b",")?;
                }
                output.write_all(b"{\"guid\":")?;
                write_json_string(&mut output, &unity_guid_string(*guid))?;
                output.write_all(b",\"paths\":[")?;
                for (path_index, path) in paths.iter().enumerate() {
                    if path_index > 0 {
                        output.write_all(b",")?;
                    }
                    write_json_string(&mut output, path)?;
                }
                output.write_all(b"]}")?;
            }
            output.write_all(b"]}\n")?;
            output.flush()
        })();
        result.map_err(|error| format!("could not write Unity metadata JSON: {error}"))?;
    } else {
        println!("Unity metadata audit: {}", root.display());
        println!("Checked paths: {checked}");
        println!("Missing .meta files: {missing_meta_count}");
        for path in missing_meta
            .iter()
            .take(if summary { 0 } else { usize::MAX })
        {
            println!("  missing: {path}");
        }
        println!("Orphan .meta files: {orphan_meta_count}");
        for path in orphan_meta
            .iter()
            .take(if summary { 0 } else { usize::MAX })
        {
            println!("  orphan: {path}");
        }
        println!("Missing GUIDs: {missing_guid_count}");
        for path in missing_guid
            .iter()
            .take(if summary { 0 } else { usize::MAX })
        {
            println!("  no guid: {path}");
        }
        println!("Malformed GUIDs: {invalid_guid_count}");
        for path in invalid_guid
            .iter()
            .take(if summary { 0 } else { usize::MAX })
        {
            println!("  invalid guid: {path}");
        }
        println!("Duplicate GUID collisions: {duplicate_guid_count}");
        for (guid, paths) in duplicate_guids
            .iter()
            .take(if summary { 0 } else { usize::MAX })
        {
            println!("  {}: {}", unity_guid_string(*guid), paths.join(", "));
        }
        if sidecars_only {
            println!("GUID checks skipped; only .meta sidecar presence was checked.");
        }
        if issue_count == 0 {
            if sidecars_only {
                println!("Unity .meta sidecars are present.");
            } else {
                println!("Unity asset metadata and GUIDs are consistent.");
            }
        }
    }
    if issue_count > 0 {
        return Err(format!("Unity metadata audit found {issue_count} issue(s)"));
    }
    Ok(())
}

fn run_gitignore(args: &[String]) -> Result<(), String> {
    const UNITY_PATTERNS: &[&str] = &[
        "[Ll]ibrary/",
        "[Tt]emp/",
        "[Oo]bj/",
        "[Bb]uild/",
        "[Bb]uilds/",
        "[Ll]ogs/",
        "[Uu]ser[Ss]ettings/",
        ".vs/",
        "*.csproj",
        "*.sln",
    ];
    const UNREAL_PATTERNS: &[&str] = &[
        "Binaries/",
        "DerivedDataCache/",
        "Intermediate/",
        ".vs/",
        "*.sln",
        "*.suo",
        "*.xcodeproj/",
        "*.xcworkspace/",
    ];
    const GODOT_PATTERNS: &[&str] = &[".godot/", ".import/"];

    let mut apply = false;
    let mut json = false;
    let mut compact = false;
    let mut path = None;
    for arg in args {
        match arg.as_str() {
            "--apply" => apply = true,
            "--json" => json = true,
            "--compact" => compact = true,
            "--help" | "-h" => {
                println!(
                    "Usage: mediaforge gitignore [PATH] [--apply] [--json] [--compact]\n\nPreview engine-specific generated-file patterns for .gitignore. Existing files are only appended to when --apply is supplied. --json emits the result and proposed or applied patterns as one object. --compact removes formatting whitespace from JSON output and requires --json."
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
    if compact && !json {
        return Err("--compact can only be used with --json".to_owned());
    }
    let path = path.unwrap_or_else(|| PathBuf::from("."));
    let root = fs::canonicalize(&path)
        .map_err(|error| format!("cannot access {}: {error}", path.display()))?;
    if !root.is_dir() {
        return Err(format!("expected a project directory: {}", root.display()));
    }

    let entries = fs::read_dir(&root)
        .map_err(|error| format!("cannot read project directory {}: {error}", root.display()))?
        .map(|entry| entry.map_err(|error| format!("cannot read project directory entry: {error}")))
        .collect::<Result<Vec<_>, _>>()?;
    let unity = is_real_directory(&root.join("Assets"))
        || is_real_file(&root.join("ProjectSettings/ProjectVersion.txt"));
    let mut unreal = false;
    for entry in &entries {
        let file_type = entry
            .file_type()
            .map_err(|error| format!("cannot inspect {}: {error}", entry.path().display()))?;
        if file_type.is_file()
            && entry
                .path()
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("uproject"))
        {
            unreal = true;
            break;
        }
    }
    let godot = is_real_file(&root.join("project.godot"));
    if !unity && !unreal && !godot {
        return Err("no Unity, Unreal, or Godot project marker found".to_owned());
    }

    let mut engines = Vec::new();
    let mut patterns = Vec::<&str>::new();
    if unity {
        engines.push("Unity");
        patterns.extend_from_slice(UNITY_PATTERNS);
    }
    if unreal {
        engines.push("Unreal Engine");
        patterns.extend_from_slice(UNREAL_PATTERNS);
    }
    if godot {
        engines.push("Godot");
        patterns.extend_from_slice(GODOT_PATTERNS);
    }
    let mut seen_patterns = HashSet::new();
    patterns.retain(|pattern| seen_patterns.insert(*pattern));

    let gitignore = root.join(".gitignore");
    let state = entry_state(&gitignore)
        .map_err(|error| format!("cannot inspect {}: {error}", gitignore.display()))?;
    if state == EntryState::Other {
        return Err(format!(
            "{} exists but is not a regular file; refusing to modify it",
            gitignore.display()
        ));
    }
    let existing = match state {
        EntryState::Missing => String::new(),
        EntryState::File => fs::read_to_string(&gitignore)
            .map_err(|error| format!("cannot read {} as UTF-8: {error}", gitignore.display()))?,
        EntryState::Other => unreachable!(),
    };
    let existing_patterns: HashSet<_> = existing.lines().map(str::trim).collect();
    let missing: Vec<_> = patterns
        .iter()
        .copied()
        .filter(|pattern| !existing_patterns.contains(pattern))
        .collect();

    let emit_json = |result: &str, added: &[&str]| {
        write_json_stdout("gitignore JSON", |output| {
            let mut output = JsonWhitespaceWriter::new(output, compact);
            writeln!(
                output,
                "{{\"schema_version\":1,\"type\":\"gitignore\",\"root\":{},\"target\":{},\"engines\":[{}],\"result\":{},\"patterns_added\":[{}]}}",
                json_string(&root.to_string_lossy()),
                json_string(&gitignore.to_string_lossy()),
                engines
                    .iter()
                    .map(|engine| json_string(engine))
                    .collect::<Vec<_>>()
                    .join(","),
                json_string(result),
                added
                    .iter()
                    .map(|pattern| json_string(pattern))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        })
    };
    if !json {
        println!("Project engines: {}", engines.join(", "));
        println!("Target: {}", gitignore.display());
    }
    if missing.is_empty() {
        if json {
            emit_json("already_current", &[])?;
        } else {
            println!("All recommended patterns are already present.");
        }
        return Ok(());
    }
    if !apply {
        if json {
            emit_json("preview", &missing)?;
        } else {
            println!("{} new pattern(s):", missing.len());
            for pattern in &missing {
                println!("  {pattern}");
            }
            if unreal {
                println!(
                    "Note: Unreal's Saved/ folder is not ignored because it can contain backups and logs."
                );
            }
            println!("Preview only. Re-run with --apply to append these patterns.");
        }
        return Ok(());
    }

    let current_state = entry_state(&gitignore)
        .map_err(|error| format!("cannot re-inspect {}: {error}", gitignore.display()))?;
    if current_state == EntryState::Other {
        return Err(format!(
            "{} changed to a non-file; refusing to modify it",
            gitignore.display()
        ));
    }
    let current = match current_state {
        EntryState::Missing => String::new(),
        EntryState::File => fs::read_to_string(&gitignore)
            .map_err(|error| format!("cannot re-read {}: {error}", gitignore.display()))?,
        EntryState::Other => unreachable!(),
    };
    if current_state != state || current != existing {
        return Err(format!(
            "{} changed after inspection; refusing to append stale suggestions",
            gitignore.display()
        ));
    }
    let current_patterns: HashSet<_> = current.lines().map(str::trim).collect();
    let to_add: Vec<_> = patterns
        .iter()
        .copied()
        .filter(|pattern| !current_patterns.contains(pattern))
        .collect();
    if to_add.is_empty() {
        if json {
            emit_json("already_current", &[])?;
        } else {
            println!("No changes needed; the patterns were added by another process.");
        }
        return Ok(());
    }

    let newline = if current.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let mut addition = String::new();
    if !current.is_empty() {
        if !current.ends_with('\n') {
            addition.push_str(newline);
        }
        if !current.trim().is_empty() {
            addition.push_str(newline);
        }
    }
    addition.push_str("# MediaForge generated files");
    addition.push_str(newline);
    for pattern in &to_add {
        addition.push_str(pattern);
        addition.push_str(newline);
    }

    match current_state {
        EntryState::Missing => {
            write_new_file_atomically(&gitignore, addition.as_bytes(), ".gitignore")?;
        }
        EntryState::File => {
            let mut file = OpenOptions::new()
                .append(true)
                .open(&gitignore)
                .map_err(|error| format!("cannot append {}: {error}", gitignore.display()))?;
            file.write_all(addition.as_bytes())
                .and_then(|()| file.sync_all())
                .map_err(|error| format!("could not append {}: {error}", gitignore.display()))?;
        }
        EntryState::Other => unreachable!(),
    }
    if json {
        emit_json("applied", &to_add)?;
    } else {
        for pattern in &to_add {
            println!("  {pattern}");
        }
        if unreal {
            println!(
                "Note: Unreal's Saved/ folder is not ignored because it can contain backups and logs."
            );
        }
        println!(
            "Appended {} pattern(s) to {}.",
            to_add.len(),
            gitignore.display()
        );
    }
    Ok(())
}

const ORGANIZE_MANIFEST: &str = ".mediaforge-undo";
const ORGANIZE_MAGIC: &[u8; 8] = b"MFORG003";
const ORGANIZE_MAGIC_V2: &[u8; 8] = b"MFORG002";
const ORGANIZE_MAGIC_V1: &[u8; 8] = b"MFORG001";
const ORGANIZE_MANIFEST_TEMP: &str = ".mediaforge-undo.tmp";
const ORGANIZE_COMPLETE_MARKER: &str = ".mediaforge-undo.complete";
const TAR_BLOCK_SIZE: usize = 512;
static TAR_ZERO_BLOCK: [u8; TAR_BLOCK_SIZE] = [0; TAR_BLOCK_SIZE];
static TAR_END_MARKER: [u8; TAR_BLOCK_SIZE * 2] = [0; TAR_BLOCK_SIZE * 2];
const TAR_OCTAL_MAX: u64 = 0o77777777777;

#[derive(Clone, Debug)]
struct PlannedMove {
    from: String,
    to: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EntryState {
    Missing,
    File,
    Other,
}

fn unity_meta_asset_key(name: &str) -> String {
    if cfg!(any(target_os = "windows", target_os = "macos")) {
        name.to_lowercase()
    } else {
        name.to_owned()
    }
}

fn collect_unity_meta_sidecars(entries: &[fs::DirEntry]) -> HashMap<String, Option<String>> {
    let mut sidecars = HashMap::<String, Option<String>>::new();
    for entry in entries {
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        let Some(asset_name) = strip_ascii_case_insensitive_suffix(file_name, ".meta") else {
            continue;
        };
        match sidecars.entry(unity_meta_asset_key(asset_name)) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(Some(file_name.to_owned()));
            }
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                entry.insert(None);
            }
        }
    }
    sidecars
}

fn output_path_collision_key(path: &Path) -> PathBuf {
    if cfg!(any(target_os = "windows", target_os = "macos")) {
        let mut key = PathBuf::new();
        for component in path.components() {
            match component {
                std::path::Component::Normal(value) => {
                    if let Some(value) = value.to_str() {
                        key.push(value.to_lowercase());
                    } else {
                        key.push(value);
                    }
                }
                component => key.push(component.as_os_str()),
            }
        }
        key
    } else {
        path.to_path_buf()
    }
}

fn mark_output_path_collisions(
    plans: &mut [(Option<PathBuf>, Result<(), String>)],
    output_kind: &str,
) {
    let mut first_by_key = HashMap::<PathBuf, usize>::new();
    for index in 0..plans.len() {
        let Some(output) = plans[index].0.as_ref() else {
            continue;
        };
        let key = output_path_collision_key(output);
        if let Some(first_index) = first_by_key.get(&key).copied() {
            let first_output = plans[first_index]
                .0
                .as_ref()
                .expect("collision key is associated with an output path");
            let error = format!(
                "multiple source files map to colliding {output_kind} paths: {} and {}",
                first_output.display(),
                output.display()
            );
            plans[first_index].1 = Err(error.clone());
            plans[index].1 = Err(error);
        } else {
            first_by_key.insert(key, index);
        }
    }
}

fn entry_state(path: &Path) -> io::Result<EntryState> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
            Ok(EntryState::File)
        }
        Ok(_) => Ok(EntryState::Other),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(EntryState::Missing),
        Err(error) => Err(error),
    }
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
    let mut portable = String::with_capacity(path.as_os_str().len());
    for component in path.components() {
        let std::path::Component::Normal(part) = component else {
            return None;
        };
        let part = part.to_str()?;
        if part
            .chars()
            .any(|character| matches!(character, '/' | '\\' | ':' | '\0'))
        {
            return None;
        }
        if !portable.is_empty() {
            portable.push('/');
        }
        portable.push_str(part);
    }
    (!portable.is_empty()).then_some(portable)
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
    let complete = root.join(ORGANIZE_COMPLETE_MARKER);
    if entry_state(&manifest).map_err(|error| format!("cannot inspect undo manifest: {error}"))?
        != EntryState::Missing
        || entry_state(&temporary)
            .map_err(|error| format!("cannot inspect temporary undo manifest: {error}"))?
            != EntryState::Missing
        || entry_state(&complete)
            .map_err(|error| format!("cannot inspect undo completion marker: {error}"))?
            != EntryState::Missing
    {
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

fn read_organize_manifest(root: &Path) -> Result<(Vec<PlannedMove>, bool), String> {
    let manifest = root.join(ORGANIZE_MANIFEST);
    let temporary = root.join(ORGANIZE_MANIFEST_TEMP);
    let path = match fs::symlink_metadata(&temporary) {
        Ok(_) => &temporary,
        Err(error) if error.kind() == io::ErrorKind::NotFound => &manifest,
        Err(error) => {
            return Err(format!("cannot inspect temporary undo manifest: {error}"));
        }
    };
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("no organize undo manifest found: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("undo manifest is not a regular file".to_owned());
    }
    let bytes = fs::read(path).map_err(|error| format!("cannot read undo manifest: {error}"))?;
    let mut cursor = Cursor::new(bytes.as_slice());
    let mut magic = [0; 8];
    cursor
        .read_exact(&mut magic)
        .map_err(|error| format!("invalid undo manifest header: {error}"))?;
    let (portable, has_completion_state) = if &magic == ORGANIZE_MAGIC {
        (true, true)
    } else if &magic == ORGANIZE_MAGIC_V2 {
        (true, false)
    } else if &magic == ORGANIZE_MAGIC_V1 {
        (false, false)
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
    Ok((moves, has_completion_state))
}

fn remove_organize_manifests(root: &Path) -> Result<(), String> {
    for name in [
        ORGANIZE_COMPLETE_MARKER,
        ORGANIZE_MANIFEST_TEMP,
        ORGANIZE_MANIFEST,
    ] {
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

fn organize_is_complete(root: &Path) -> Result<bool, String> {
    let path = root.join(ORGANIZE_COMPLETE_MARKER);
    match entry_state(&path)
        .map_err(|error| format!("cannot inspect undo completion marker: {error}"))?
    {
        EntryState::Missing => Ok(false),
        EntryState::Other => Err("undo completion marker is not a regular file".to_owned()),
        EntryState::File => Ok(true),
    }
}

fn write_organize_complete_marker(root: &Path) -> Result<(), String> {
    let path = root.join(ORGANIZE_COMPLETE_MARKER);
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|error| format!("cannot create undo completion marker: {error}"))?;
    file.sync_all()
        .map_err(|error| format!("could not persist undo completion marker: {error}"))
}

fn size_category(size: u64) -> &'static str {
    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;
    match size {
        size if size < MIB => "size-under-1MiB",
        size if size < 64 * MIB => "size-1-64MiB",
        size if size < 512 * MIB => "size-64-512MiB",
        size if size < 2 * GIB => "size-512MiB-2GiB",
        _ => "size-2GiB-plus",
    }
}

fn size_category_order(category: &str) -> u8 {
    match category {
        "size-under-1MiB" => 0,
        "size-1-64MiB" => 1,
        "size-64-512MiB" => 2,
        "size-512MiB-2GiB" => 3,
        "size-2GiB-plus" => 4,
        _ => u8::MAX,
    }
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

fn move_file_case_only(from: &Path, to: &Path) -> io::Result<()> {
    let parent = from
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "source has no parent"))?;
    let mut temporary = None;
    for attempt in 0..1000_u32 {
        let candidate = parent.join(format!(
            ".mediaforge-rename-{}-{attempt}.tmp",
            std::process::id()
        ));
        if entry_state(&candidate)? == EntryState::Missing {
            temporary = Some(candidate);
            break;
        }
    }
    let temporary = temporary.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not find a free temporary name for case-only rename",
        )
    })?;
    move_file_no_replace(from, &temporary)?;
    if let Err(error) = move_file_no_replace(&temporary, to) {
        return match move_file_no_replace(&temporary, from) {
            Ok(()) => Err(error),
            Err(rollback_error) => Err(io::Error::other(format!(
                "case-only rename failed ({error}) and source rollback failed ({rollback_error}); file remains at {}",
                temporary.display()
            ))),
        };
    }
    Ok(())
}

fn paths_resolve_same_file(left: &Path, right: &Path) -> io::Result<bool> {
    Ok(fs::canonicalize(left)? == fs::canonicalize(right)?)
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
    let mut json = false;
    let mut compact = false;
    let mut summary_only = false;
    let mut kind_filter = None;
    let mut extension_filter = None;
    let mut min_size = 0_u64;
    let mut include_hidden = false;
    let mut include_generated = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--by" => {
                by = iter
                    .next()
                    .ok_or_else(|| "--by requires 'type', 'extension', or 'size'".to_owned())?;
                if by != "type" && by != "extension" && by != "size" {
                    return Err("--by must be 'type', 'extension', or 'size'".to_owned());
                }
            }
            "--include-hidden" => include_hidden = true,
            "--include-generated" => include_generated = true,
            "--type" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--type requires a value".to_owned())?;
                kind_filter = Some(parse_asset_kind(value).ok_or_else(|| {
                    format!(
                        "unknown asset type '{value}'; choose video, audio, image, model, texture, project, or other"
                    )
                })?);
            }
            "--extension" => {
                let value = iter.next().ok_or_else(|| {
                    "--extension requires a comma-separated extension list".to_owned()
                })?;
                extension_filter = Some(parse_extensions(value)?);
            }
            "--min-size" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--min-size requires a value".to_owned())?;
                min_size = parse_size(value)?;
            }
            "--apply" => apply = true,
            "--json" => json = true,
            "--compact" => compact = true,
            "--summary" => summary_only = true,
            "--help" | "-h" => {
                println!(
                    "Usage: mediaforge organize [PATH] [--by type|extension|size] [--type TYPE] [--extension EXTENSIONS] [--min-size SIZE] [--include-hidden] [--include-generated] [--json] [--compact] [--summary] [--apply]\n\nPreview an asset organization plan. Matching .meta sidecars move with their selected assets. --by size groups files into <1 MiB, 1–64 MiB, 64–512 MiB, 512 MiB–2 GiB, and >=2 GiB buckets. --type and --extension select files; --min-size uses bytes or K/M/G/T binary units. Hidden files are skipped unless --include-hidden is set. Unity, Unreal, and Godot generated folders are skipped unless --include-generated is set. --json emits the plan and skipped count as JSON. --compact removes formatting whitespace from JSON output and requires --json. --summary requires --json and omits move paths while keeping counts. Files are only moved with --apply; a manifest is written so the move can be undone."
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
    if compact && !json {
        return Err("--compact can only be used with --json".to_owned());
    }
    if summary_only && !json {
        return Err("--summary can only be used with --json".to_owned());
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

    let ignored = ignored_by_git(&root)?;
    let mut records = Vec::new();
    let mut skip_dirs = SKIP_DIRS.to_vec();
    if !include_generated {
        skip_dirs.extend_from_slice(PACK_GENERATED_DIRS);
    }
    let kinds = kind_filter.map(|kind| [kind]);
    let filters = StrictWalkFilters {
        kinds: kinds.as_ref().map(|values| values.as_slice()),
        extensions: extension_filter.as_ref(),
        min_size: Some(min_size),
        omit_extension: by != "extension",
        store_relative_path: true,
        ..StrictWalkFilters::default()
    };
    walk_strict_filtered(
        &root,
        &root,
        include_hidden,
        &skip_dirs,
        &ignored,
        &filters,
        &mut records,
    )?;
    records.sort_unstable_by(|left, right| left.relative.cmp(&right.relative));
    let mut moves = Vec::<PlannedMove>::new();
    let mut skipped = 0usize;
    let mut destinations = HashSet::<PathBuf>::new();
    let mut paired_assets = HashSet::<PathBuf>::new();
    for record in records {
        let relative = record.path.as_path();
        let Some(from_text) = portable_relative_path(relative) else {
            skipped += 1;
            continue;
        };
        if unity_meta_target(relative).is_some_and(|asset| paired_assets.remove(&asset)) {
            continue;
        }
        let sidecar_relative = unity_meta_sidecar(relative);
        let sidecar_source = root.join(&sidecar_relative);
        let sidecar_state = entry_state(&sidecar_source).map_err(|error| {
            format!(
                "cannot inspect sidecar {}: {error}",
                sidecar_source.display()
            )
        })?;
        if sidecar_state == EntryState::Other {
            skipped += 1;
            continue;
        }
        if sidecar_state == EntryState::File {
            paired_assets.insert(relative.to_path_buf());
        }
        let category = match by {
            "type" => record.kind.as_str().to_owned(),
            "extension" if record.extension == "[no extension]" => "no-extension".to_owned(),
            "extension" => record.extension.trim_start_matches('.').to_owned(),
            "size" => size_category(record.size).to_owned(),
            _ => unreachable!("organize grouping was validated"),
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
        let sidecar_to = PathBuf::from(&category).join(&sidecar_relative);
        let sidecar_pair = if sidecar_state == EntryState::File {
            let Some(sidecar_from_text) = portable_relative_path(&sidecar_relative) else {
                skipped += 1;
                continue;
            };
            let Some(sidecar_to_text) = portable_relative_path(&sidecar_to) else {
                skipped += 1;
                continue;
            };
            let sidecar_from = validate_portable_relative_path(&sidecar_from_text)?;
            let sidecar_to_path = validate_portable_relative_path(&sidecar_to_text)?;
            Some((
                sidecar_from_text,
                sidecar_to_text,
                sidecar_from,
                sidecar_to_path,
            ))
        } else {
            None
        };
        let destination_state = entry_state(&root.join(&to))
            .map_err(|error| format!("cannot inspect destination {}: {error}", to.display()))?;
        let sidecar_destination_state = if sidecar_pair.is_some() {
            entry_state(&root.join(&sidecar_to)).map_err(|error| {
                format!(
                    "cannot inspect sidecar destination {}: {error}",
                    sidecar_to.display()
                )
            })?
        } else {
            EntryState::Missing
        };
        if destinations.contains(&to)
            || destination_state != EntryState::Missing
            || (sidecar_pair.is_some()
                && (destinations.contains(&sidecar_to)
                    || sidecar_destination_state != EntryState::Missing))
        {
            skipped += 1;
            continue;
        }
        if reject_symlink_components(&root, &from).is_err()
            || reject_symlink_components(&root, &to).is_err()
            || sidecar_pair.as_ref().is_some_and(|(_, _, from, to)| {
                reject_symlink_components(&root, from).is_err()
                    || reject_symlink_components(&root, to).is_err()
            })
        {
            skipped += 1;
            continue;
        }
        destinations.insert(to);
        if let Some((sidecar_from_text, sidecar_to_text, _, _)) = sidecar_pair {
            destinations.insert(sidecar_to);
            moves.push(PlannedMove {
                from: sidecar_from_text,
                to: sidecar_to_text,
            });
        }
        moves.push(PlannedMove {
            from: from_text,
            to: to_text,
        });
    }

    if json && !apply {
        write_json_stdout("organize JSON", |output| {
            let mut output = JsonWhitespaceWriter::new(output, compact);
            if summary_only {
                return writeln!(
                    &mut output,
                    "{{\"root\":{},\"by\":{},\"include_hidden\":{},\"include_generated\":{},\"applied\":false,\"moves\":null,\"moves_included\":false,\"move_count\":{},\"skipped_count\":{}}}",
                    json_string(&root.to_string_lossy()),
                    json_string(by),
                    include_hidden,
                    include_generated,
                    moves.len(),
                    skipped
                );
            }
            writeln!(
                &mut output,
                "{{\n  \"root\": {},\n  \"by\": {},\n  \"include_hidden\": {},\n  \"include_generated\": {},\n  \"applied\": {},\n  \"moves\": [",
                json_string(&root.to_string_lossy()),
                json_string(by),
                if include_hidden { "true" } else { "false" },
                if include_generated { "true" } else { "false" },
                if apply { "true" } else { "false" }
            )?;
            for (index, movement) in moves.iter().enumerate() {
                let comma = if index + 1 == moves.len() { "" } else { "," };
                writeln!(
                    &mut output,
                    "    {{\"from\": {}, \"to\": {}}}{}",
                    json_string(&movement.from),
                    json_string(&movement.to),
                    comma
                )?;
            }
            writeln!(
                &mut output,
                "  ],\n  \"move_count\": {},\n  \"skipped_count\": {}\n}}",
                moves.len(),
                skipped
            )
        })?;
    } else if !json {
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
    }
    if !apply {
        if !moves.is_empty() && !json {
            println!(
                "\nPreview only. Re-run with --apply to move files; then use 'mediaforge undo {}' to restore them.",
                root.display()
            );
        }
        return Ok(());
    }
    if moves.is_empty() {
        if json {
            write_json_stdout("organize JSON", |output| {
                let mut output = JsonWhitespaceWriter::new(output, compact);
                writeln!(
                    &mut output,
                    "{{\"root\": {}, \"by\": {}, \"include_hidden\": {}, \"include_generated\": {}, \"applied\": {}, \"moves\": [], \"move_count\": 0, \"moved_count\": 0, \"skipped_count\": {}, \"result\": \"no_changes\"}}",
                    json_string(&root.to_string_lossy()),
                    json_string(by),
                    if include_hidden { "true" } else { "false" },
                    if include_generated { "true" } else { "false" },
                    if apply { "true" } else { "false" },
                    skipped
                )
            })?;
        } else {
            println!("No files need moving.");
        }
        return Ok(());
    }

    for movement in &moves {
        let from = validate_relative_path(&movement.from)?;
        let to = validate_relative_path(&movement.to)?;
        reject_symlink_components(&root, &from)?;
        reject_symlink_components(&root, &to)?;
        let source_state = entry_state(&root.join(&from))
            .map_err(|error| format!("cannot inspect source {}: {error}", from.display()))?;
        let destination_state = entry_state(&root.join(&to))
            .map_err(|error| format!("cannot inspect destination {}: {error}", to.display()))?;
        if source_state != EntryState::File || destination_state != EntryState::Missing {
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
    if let Err(error) = write_organize_complete_marker(&root) {
        return Err(format!(
            "moved {moved} file(s), but could not mark organization complete: {error}; recovery manifest remains at {}",
            root.join(ORGANIZE_MANIFEST).display()
        ));
    }
    if json {
        write_json_stdout("organize JSON", |output| {
            let mut output = JsonWhitespaceWriter::new(output, compact);
            if summary_only {
                return writeln!(
                    &mut output,
                    "{{\"root\":{},\"by\":{},\"include_hidden\":{},\"include_generated\":{},\"applied\":true,\"moves\":null,\"moves_included\":false,\"move_count\":{},\"moved_count\":{},\"skipped_count\":{},\"result\":\"applied\",\"undo_manifest\":{}}}",
                    json_string(&root.to_string_lossy()),
                    json_string(by),
                    include_hidden,
                    include_generated,
                    moves.len(),
                    moved,
                    skipped,
                    json_string(&root.join(ORGANIZE_MANIFEST).to_string_lossy())
                );
            }
            writeln!(
                &mut output,
                "{{\n  \"root\": {},\n  \"by\": {},\n  \"include_hidden\": {},\n  \"include_generated\": {},\n  \"applied\": true,\n  \"moves\": [",
                json_string(&root.to_string_lossy()),
                json_string(by),
                if include_hidden { "true" } else { "false" },
                if include_generated { "true" } else { "false" }
            )?;
            for (index, movement) in moves.iter().enumerate() {
                let comma = if index + 1 == moves.len() { "" } else { "," };
                writeln!(
                    &mut output,
                    "    {{\"from\": {}, \"to\": {}}}{}",
                    json_string(&movement.from),
                    json_string(&movement.to),
                    comma
                )?;
            }
            writeln!(
                &mut output,
                "  ],\n  \"move_count\": {},\n  \"moved_count\": {},\n  \"skipped_count\": {},\n  \"result\": \"applied\",\n  \"undo_manifest\": {}\n}}",
                moves.len(),
                moved,
                skipped,
                json_string(&root.join(ORGANIZE_MANIFEST).to_string_lossy())
            )
        })?;
    } else {
        println!(
            "Moved {moved} file(s). Undo manifest: {}",
            root.join(ORGANIZE_MANIFEST).display()
        );
    }
    Ok(())
}

fn run_undo(args: &[String]) -> Result<(), String> {
    let mut path = None;
    let mut json = false;
    let mut compact = false;
    for arg in args {
        match arg.as_str() {
            "--json" => json = true,
            "--compact" => compact = true,
            "--help" | "-h" => {
                println!(
                    "Usage: mediaforge undo [PATH] [--json] [--compact]\n\nRestore files moved by the most recent 'organize --apply', 'rename --apply', or 'sequence --apply' in PATH. --json emits restored and conflict counts. --compact removes formatting whitespace from JSON output and requires --json."
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
    if compact && !json {
        return Err("--compact can only be used with --json".to_owned());
    }
    let path = path.unwrap_or_else(|| PathBuf::from("."));
    let root = fs::canonicalize(&path)
        .map_err(|error| format!("cannot access {}: {error}", path.display()))?;
    if !root.is_dir() {
        return Err(format!("expected a directory: {}", root.display()));
    }
    let (moves, has_completion_state) = read_organize_manifest(&root)?;
    let organize_complete = !has_completion_state || organize_is_complete(&root)?;
    let mut restored = 0usize;
    let mut conflicts = 0usize;
    for movement in moves.iter().rev() {
        let from = root.join(validate_relative_path(&movement.from)?);
        let to = root.join(validate_relative_path(&movement.to)?);
        reject_symlink_components(&root, &PathBuf::from(&movement.from))?;
        reject_symlink_components(&root, &PathBuf::from(&movement.to))?;
        let from_state = entry_state(&from)
            .map_err(|error| format!("cannot inspect original path {}: {error}", from.display()))?;
        let to_state = entry_state(&to)
            .map_err(|error| format!("cannot inspect organized path {}: {error}", to.display()))?;
        if from_state == EntryState::Missing && to_state == EntryState::File {
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
        } else if from_state == EntryState::File
            && to_state == EntryState::File
            && movement.from.to_lowercase() == movement.to.to_lowercase()
            && paths_resolve_same_file(&from, &to).map_err(|error| {
                format!(
                    "cannot compare {} and {}: {error}",
                    from.display(),
                    to.display()
                )
            })?
        {
            if let Err(error) = move_file_case_only(&to, &from) {
                eprintln!("mediaforge: could not restore {}: {error}", movement.from);
                conflicts += 1;
            } else {
                restored += 1;
            }
        } else if from_state == EntryState::File && to_state == EntryState::File {
            eprintln!(
                "mediaforge: conflict; both source and organized file exist: {}",
                movement.from
            );
            conflicts += 1;
        } else if from_state == EntryState::Missing && to_state == EntryState::Missing {
            eprintln!(
                "mediaforge: missing both source and organized file: {}",
                movement.from
            );
            conflicts += 1;
        } else if organize_complete
            && from_state == EntryState::File
            && to_state == EntryState::Missing
        {
            eprintln!(
                "mediaforge: conflict; organized file is missing but the original path is occupied: {}",
                movement.from
            );
            conflicts += 1;
        } else if from_state == EntryState::Other || to_state == EntryState::Other {
            eprintln!(
                "mediaforge: conflict; an expected file path is occupied by a non-file: {}",
                movement.from
            );
            conflicts += 1;
        }
    }
    let manifest_retained = conflicts != 0;
    if !manifest_retained {
        remove_organize_manifests(&root)?;
    }
    if json {
        write_json_stdout("undo JSON", |output| {
            let mut output = JsonWhitespaceWriter::new(output, compact);
            writeln!(
                output,
                "{{\n  \"root\": {},\n  \"restored_count\": {},\n  \"conflict_count\": {},\n  \"manifest_retained\": {},\n  \"result\": {}\n}}",
                json_string(&root.to_string_lossy()),
                restored,
                conflicts,
                if manifest_retained { "true" } else { "false" },
                json_string(if manifest_retained {
                    "partial"
                } else {
                    "restored"
                })
            )
        })?;
    } else if manifest_retained {
        println!(
            "Restored {restored} file(s); {conflicts} conflict(s) remain. Manifest retained for recovery."
        );
    } else {
        println!("Restored {restored} file(s); undo manifest removed.");
    }
    Ok(())
}

fn run_rename(args: &[String]) -> Result<(), String> {
    let mut path = None;
    let mut replacements = Vec::<(String, String)>::new();
    let mut prefix = String::new();
    let mut suffix = String::new();
    let mut extension_case = "keep";
    let mut extension_filter = None;
    let mut include_hidden = false;
    let mut apply = false;
    let mut json = false;
    let mut compact = false;
    let mut summary_only = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--replace" => {
                let old = iter
                    .next()
                    .ok_or_else(|| "--replace requires OLD and NEW".to_owned())?;
                let new = iter
                    .next()
                    .ok_or_else(|| "--replace requires OLD and NEW".to_owned())?;
                if old.is_empty() {
                    return Err("--replace OLD cannot be empty".to_owned());
                }
                replacements.push((old.clone(), new.clone()));
            }
            "--prefix" => {
                prefix = iter
                    .next()
                    .ok_or_else(|| "--prefix requires text".to_owned())?
                    .clone()
            }
            "--suffix" => {
                suffix = iter
                    .next()
                    .ok_or_else(|| "--suffix requires text".to_owned())?
                    .clone()
            }
            "--extension-case" => {
                extension_case = iter
                    .next()
                    .ok_or_else(|| "--extension-case requires keep, lower, or upper".to_owned())?;
                if !matches!(extension_case, "keep" | "lower" | "upper") {
                    return Err("--extension-case must be keep, lower, or upper".to_owned());
                }
            }
            "--extension" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--extension requires a comma-separated list".to_owned())?;
                extension_filter = Some(parse_extensions(value)?);
            }
            "--include-hidden" => include_hidden = true,
            "--apply" => apply = true,
            "--json" => json = true,
            "--compact" => compact = true,
            "--summary" => summary_only = true,
            "--help" | "-h" => {
                println!(
                    "Usage: mediaforge rename [PATH] [--replace OLD NEW] [--prefix TEXT] [--suffix TEXT] [--extension-case keep|lower|upper] [--extension EXTENSIONS] [--include-hidden] [--json] [--compact] [--summary] [--apply]\n\nPreview filename changes for files directly inside PATH. Repeated --replace rules run in order; prefix and suffix apply to the filename stem. Matching .meta sidecars are included and moved before their assets to preserve Unity GUIDs. Extensions are preserved unless --extension-case changes their case. --compact removes formatting whitespace from JSON output and requires --json. --summary requires --json and omits move paths and skipped examples while keeping counts. The default is preview; --apply renames files and records an undo manifest for 'mediaforge undo PATH'."
                );
                return Ok(());
            }
            value if value.starts_with('-') => return Err(format!("unknown option: {value}")),
            value => {
                if path.is_some() {
                    return Err("only one rename directory may be provided".into());
                }
                path = Some(PathBuf::from(value));
            }
        }
    }
    if compact && !json {
        return Err("--compact can only be used with --json".to_owned());
    }
    if summary_only && !json {
        return Err("--summary can only be used with --json".to_owned());
    }
    if replacements.is_empty() && prefix.is_empty() && suffix.is_empty() && extension_case == "keep"
    {
        return Err(
            "specify at least one rename rule (--replace, --prefix, --suffix, or --extension-case)"
                .to_owned(),
        );
    }
    let path = path.unwrap_or_else(|| PathBuf::from("."));
    let root = fs::canonicalize(&path)
        .map_err(|error| format!("cannot access {}: {error}", path.display()))?;
    if !root.is_dir() {
        return Err(format!("expected a directory: {}", root.display()));
    }
    if apply
        && (fs::symlink_metadata(root.join(ORGANIZE_MANIFEST_TEMP)).is_ok()
            || fs::symlink_metadata(root.join(ORGANIZE_MANIFEST)).is_ok())
    {
        return Err(format!(
            "an undo manifest or recovery file already exists under {}; run 'mediaforge undo {}' first",
            root.display(),
            root.display()
        ));
    }
    let ignored = ignored_by_git(&root)?;
    let mut entries = fs::read_dir(&root)
        .map_err(|error| format!("cannot list {}: {error}", root.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("cannot read directory entry: {error}"))?;
    sort_entries(&mut entries);
    let sidecars_by_asset = collect_unity_meta_sidecars(&entries);
    let mut moves = Vec::<PlannedMove>::new();
    let mut skipped = 0_usize;
    let mut collision_examples = Vec::<(String, String)>::new();
    let mut collision_count = 0_usize;
    let mut planned_targets = HashSet::<String>::new();
    for entry in entries {
        let source = entry.path();
        let name = entry.file_name();
        let Some(name_text) = name.to_str() else {
            skipped += 1;
            continue;
        };
        if (!include_hidden && is_dot_hidden(&source))
            || is_ignored(name_text, &ignored)
            || matches!(
                name_text,
                ".gitignore"
                    | ORGANIZE_MANIFEST
                    | ORGANIZE_MANIFEST_TEMP
                    | ORGANIZE_COMPLETE_MARKER
            )
        {
            skipped += 1;
            continue;
        }
        if let Some(asset_name) = strip_ascii_case_insensitive_suffix(name_text, ".meta") {
            let asset_path = root.join(asset_name);
            match fs::symlink_metadata(&asset_path) {
                Ok(asset_metadata)
                    if !asset_metadata.file_type().is_symlink()
                        && (asset_metadata.is_file() || asset_metadata.is_dir()) =>
                {
                    if asset_metadata.is_dir() {
                        skipped += 1;
                    }
                    continue;
                }
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!(
                        "cannot inspect Unity asset {}: {error}",
                        asset_path.display()
                    ));
                }
            }
        }
        let metadata = fs::symlink_metadata(&source)
            .map_err(|error| format!("cannot inspect {}: {error}", source.display()))?;
        if (!include_hidden && metadata_has_hidden_attribute(&metadata))
            || !metadata.is_file()
            || metadata.file_type().is_symlink()
        {
            skipped += 1;
            continue;
        }
        let extension = source.extension().and_then(|value| value.to_str());
        if extension_filter
            .as_ref()
            .is_some_and(|extensions| !matches_extension_filter(extension, extensions))
        {
            continue;
        }
        let original_extension = source
            .extension()
            .and_then(|value| value.to_str())
            .map(|value| format!(".{value}"))
            .unwrap_or_default();
        let stem = source
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or(name_text);
        let mut new_stem = stem.to_owned();
        for (old, new) in &replacements {
            new_stem = new_stem.replace(old, new);
        }
        new_stem = format!("{prefix}{new_stem}{suffix}");
        let new_extension = match extension_case {
            "lower" => original_extension.to_ascii_lowercase(),
            "upper" => original_extension.to_ascii_uppercase(),
            _ => original_extension,
        };
        let target_name = format!("{new_stem}{new_extension}");
        let sidecar_name = match sidecars_by_asset.get(&unity_meta_asset_key(name_text)) {
            Some(Some(sidecar_name)) => sidecar_name.clone(),
            Some(None) => {
                skipped += 1;
                collision_count += 1;
                if collision_examples.len() < 100 {
                    collision_examples.push((name_text.to_owned(), "ambiguous_sidecar".to_owned()));
                }
                continue;
            }
            None => format!("{name_text}.meta"),
        };
        let sidecar = root.join(&sidecar_name);
        let sidecar_state = entry_state(&sidecar)
            .map_err(|error| format!("cannot inspect sidecar {}: {error}", sidecar.display()))?;
        if sidecar_state == EntryState::Other {
            skipped += 1;
            collision_count += 1;
            if collision_examples.len() < 100 {
                collision_examples.push((name_text.to_owned(), "sidecar_not_regular".to_owned()));
            }
            continue;
        }
        let sidecar_target_name = format!("{target_name}.meta");
        if validate_archive_component(&target_name).is_err()
            || target_name.encode_utf16().count() > 255
            || (sidecar_state == EntryState::File
                && (validate_archive_component(&sidecar_target_name).is_err()
                    || sidecar_target_name.encode_utf16().count() > 255))
        {
            skipped += 1;
            collision_count += 1;
            if collision_examples.len() < 100 {
                collision_examples
                    .push((name_text.to_owned(), "invalid_portable_filename".to_owned()));
            }
            continue;
        }
        let from_text = portable_relative_path(Path::new(name_text));
        let to_text = portable_relative_path(Path::new(&target_name));
        let (Some(from_text), Some(to_text)) = (from_text, to_text) else {
            skipped += 1;
            continue;
        };
        if from_text == to_text {
            continue;
        }
        let sidecar_target_key = sidecar_target_name.to_lowercase();
        if planned_targets.contains(&target_name.to_lowercase())
            || (sidecar_state == EntryState::File && planned_targets.contains(&sidecar_target_key))
        {
            skipped += 1;
            collision_count += 1;
            if collision_examples.len() < 100 {
                collision_examples.push((from_text, "duplicate_destination".to_owned()));
            }
            continue;
        }
        let target = root.join(&target_name);
        let state = entry_state(&target)
            .map_err(|error| format!("cannot inspect target {}: {error}", target.display()))?;
        let case_only_rename = state == EntryState::File
            && from_text.to_lowercase() == to_text.to_lowercase()
            && paths_resolve_same_file(&source, &target).map_err(|error| {
                format!(
                    "cannot compare {} and {}: {error}",
                    source.display(),
                    target.display()
                )
            })?;
        if state != EntryState::Missing && !case_only_rename {
            skipped += 1;
            collision_count += 1;
            if collision_examples.len() < 100 {
                collision_examples.push((from_text, "destination_exists".to_owned()));
            }
            continue;
        }
        let sidecar_target = root.join(&sidecar_target_name);
        let sidecar_case_only = if sidecar_state == EntryState::File {
            let target_state = entry_state(&sidecar_target).map_err(|error| {
                format!(
                    "cannot inspect sidecar target {}: {error}",
                    sidecar_target.display()
                )
            })?;
            target_state == EntryState::File
                && sidecar_name.to_lowercase() == sidecar_target_name.to_lowercase()
                && paths_resolve_same_file(&sidecar, &sidecar_target).map_err(|error| {
                    format!(
                        "cannot compare {} and {}: {error}",
                        sidecar.display(),
                        sidecar_target.display()
                    )
                })?
        } else {
            true
        };
        let sidecar_target_state = if sidecar_state == EntryState::File {
            entry_state(&sidecar_target).map_err(|error| {
                format!(
                    "cannot inspect sidecar target {}: {error}",
                    sidecar_target.display()
                )
            })?
        } else {
            EntryState::Missing
        };
        if sidecar_state == EntryState::File
            && sidecar_target_state != EntryState::Missing
            && !sidecar_case_only
        {
            skipped += 1;
            collision_count += 1;
            if collision_examples.len() < 100 {
                collision_examples.push((sidecar_name, "sidecar_destination_exists".to_owned()));
            }
            continue;
        }
        planned_targets.insert(target_name.to_lowercase());
        if sidecar_state == EntryState::File {
            planned_targets.insert(sidecar_target_key);
        }
        if sidecar_state == EntryState::File {
            let sidecar_from = portable_relative_path(Path::new(&sidecar_name))
                .ok_or_else(|| format!("sidecar path is not portable: {sidecar_name}"))?;
            let sidecar_to =
                portable_relative_path(Path::new(&sidecar_target_name)).ok_or_else(|| {
                    format!("sidecar target path is not portable: {sidecar_target_name}")
                })?;
            moves.push(PlannedMove {
                from: sidecar_from,
                to: sidecar_to,
            });
        }
        moves.push(PlannedMove {
            from: from_text,
            to: to_text,
        });
    }
    if json && !apply {
        write_json_stdout("rename JSON", |output| {
            let mut output = JsonWhitespaceWriter::new(output, compact);
            if summary_only {
                return writeln!(
                    output,
                    "{{\"root\":{},\"applied\":false,\"moves\":null,\"moves_included\":false,\"move_count\":{},\"skipped_count\":{},\"collision_count\":{collision_count},\"skipped_examples\":null,\"skipped_examples_included\":false,\"skipped_examples_truncated\":{}}}",
                    json_string(&root.to_string_lossy()),
                    moves.len(),
                    skipped,
                    collision_count > collision_examples.len()
                );
            }
            writeln!(
                output,
                "{{\n  \"root\": {},\n  \"applied\": false,\n  \"moves\": [",
                json_string(&root.to_string_lossy())
            )?;
            for (position, movement) in moves.iter().enumerate() {
                writeln!(
                    output,
                    "    {{\"from\": {}, \"to\": {}}}{}",
                    json_string(&movement.from),
                    json_string(&movement.to),
                    if position + 1 == moves.len() { "" } else { "," }
                )?;
            }
            writeln!(
                output,
                "  ],\n  \"move_count\": {},\n  \"skipped_count\": {},\n  \"collision_count\": {collision_count},\n  \"skipped_examples\": [",
                moves.len(),
                skipped
            )?;
            for (position, (path, reason)) in collision_examples.iter().enumerate() {
                let comma = if position + 1 == collision_examples.len() {
                    ""
                } else {
                    ","
                };
                writeln!(
                    output,
                    "    {{\"path\": {}, \"reason\": {}}}{}",
                    json_string(path),
                    json_string(reason),
                    comma
                )?;
            }
            writeln!(
                output,
                "  ],\n  \"skipped_examples_truncated\": {}\n}}",
                collision_count > collision_examples.len()
            )
        })?;
    } else if !json {
        println!(
            "Rename preview: {} file(s) to rename; {skipped} skipped",
            moves.len()
        );
        for movement in moves.iter().take(30) {
            println!("  {} -> {}", movement.from, movement.to);
        }
        if moves.len() > 30 {
            println!("  ... and {} more", moves.len() - 30);
        }
    }
    if !apply {
        if !moves.is_empty() && !json {
            println!(
                "\nPreview only. Re-run with --apply to rename files; use 'mediaforge undo {}' to restore them.",
                root.display()
            );
        }
        return Ok(());
    }
    if moves.is_empty() {
        if json {
            write_json_stdout("rename JSON", |output| {
                let mut output = JsonWhitespaceWriter::new(output, compact);
                writeln!(
                    output,
                    "{{\"root\":{},\"applied\":true,\"moves\":[],\"move_count\":0,\"renamed_count\":0,\"skipped_count\":{skipped},\"result\":\"no_changes\"}}",
                    json_string(&root.to_string_lossy())
                )
            })?;
        } else {
            println!("No files need renaming.");
        }
        return Ok(());
    }
    for movement in &moves {
        let from = validate_relative_path(&movement.from)?;
        let to = validate_relative_path(&movement.to)?;
        reject_symlink_components(&root, &from)?;
        reject_symlink_components(&root, &to)?;
        let source_path = root.join(&from);
        let target_path = root.join(&to);
        let source_state = entry_state(&source_path)
            .map_err(|error| format!("cannot inspect source {}: {error}", from.display()))?;
        let target_state = entry_state(&target_path)
            .map_err(|error| format!("cannot inspect target {}: {error}", to.display()))?;
        let case_only_rename = target_state == EntryState::File
            && movement.from.to_lowercase() == movement.to.to_lowercase()
            && paths_resolve_same_file(&source_path, &target_path).map_err(|error| {
                format!(
                    "cannot compare {} and {}: {error}",
                    source_path.display(),
                    target_path.display()
                )
            })?;
        if source_state != EntryState::File
            || (target_state != EntryState::Missing && !case_only_rename)
        {
            return Err(format!(
                "file plan changed since preview; refusing to apply: {}",
                movement.from
            ));
        }
    }
    write_organize_manifest(&root, &moves)?;
    let mut renamed = 0_usize;
    for movement in &moves {
        let from = root.join(validate_relative_path(&movement.from)?);
        let to = root.join(validate_relative_path(&movement.to)?);
        let case_only_rename = entry_state(&to)
            .map_err(|error| format!("cannot inspect target {}: {error}", to.display()))?
            == EntryState::File
            && paths_resolve_same_file(&from, &to).map_err(|error| {
                format!(
                    "cannot compare {} and {}: {error}",
                    from.display(),
                    to.display()
                )
            })?;
        let move_result = if case_only_rename {
            move_file_case_only(&from, &to)
        } else {
            move_file_no_replace(&from, &to)
        };
        if let Err(error) = move_result {
            return Err(format!(
                "renamed {renamed} file(s) before failure: {error}; recovery manifest remains at {}",
                root.join(ORGANIZE_MANIFEST).display()
            ));
        }
        renamed += 1;
    }
    write_organize_complete_marker(&root)?;
    if json {
        write_json_stdout("rename JSON", |output| {
            let mut output = JsonWhitespaceWriter::new(output, compact);
            writeln!(
                output,
                "{{\"root\":{},\"applied\":true,\"move_count\":{},\"renamed_count\":{renamed},\"skipped_count\":{skipped},\"result\":\"applied\",\"undo_manifest\":{}}}",
                json_string(&root.to_string_lossy()),
                moves.len(),
                json_string(&root.join(ORGANIZE_MANIFEST).to_string_lossy())
            )
        })?;
    } else {
        println!(
            "Renamed {renamed} file(s). Undo manifest: {}",
            root.join(ORGANIZE_MANIFEST).display()
        );
    }
    Ok(())
}

fn run_sequence(args: &[String]) -> Result<(), String> {
    let mut path = None;
    let mut prefix = "frame".to_owned();
    let mut start = 1_u64;
    let mut padding = 4_usize;
    let mut extension_filter = None;
    let mut include_hidden = false;
    let mut natural = false;
    let mut apply = false;
    let mut json = false;
    let mut compact = false;
    let mut summary_only = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--prefix" => {
                prefix = iter
                    .next()
                    .ok_or_else(|| "--prefix requires a name".to_owned())?
                    .clone();
            }
            "--start" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--start requires a number".to_owned())?;
                start = value
                    .parse::<u64>()
                    .map_err(|_| format!("invalid starting number '{value}'"))?;
            }
            "--padding" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--padding requires a number".to_owned())?;
                padding = value
                    .parse::<usize>()
                    .map_err(|_| format!("invalid number padding '{value}'"))?;
            }
            "--extension" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--extension requires a comma-separated list".to_owned())?;
                extension_filter = Some(parse_extensions(value)?);
            }
            "--include-hidden" => include_hidden = true,
            "--natural" => natural = true,
            "--apply" => apply = true,
            "--json" => json = true,
            "--compact" => compact = true,
            "--summary" => summary_only = true,
            "--help" | "-h" => {
                println!(
                    "Usage: mediaforge sequence [PATH] [--prefix NAME] [--start N] [--padding N] [--extension EXTENSIONS] [--include-hidden] [--natural] [--json] [--compact] [--summary] [--apply]\n\nPreview a stable renumbering of files directly inside PATH. Alphabetical order is the default; --natural sorts digit runs by numeric value (for example frame2 before frame10). Extensions are preserved. Matching Unity .meta sidecars move with their assets to preserve GUIDs. --compact removes formatting whitespace from JSON output and requires --json. --summary requires --json and omits move paths and skipped examples while keeping counts. The operation skips destination collisions and can be undone with 'mediaforge undo PATH'."
                );
                return Ok(());
            }
            value if value.starts_with('-') => return Err(format!("unknown option: {value}")),
            value => {
                if path.is_some() {
                    return Err("only one sequence directory may be provided".into());
                }
                path = Some(PathBuf::from(value));
            }
        }
    }
    if compact && !json {
        return Err("--compact can only be used with --json".to_owned());
    }
    if summary_only && !json {
        return Err("--summary can only be used with --json".to_owned());
    }
    if prefix.is_empty()
        || !prefix
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err("--prefix may contain only ASCII letters, digits, '_' and '-'".to_owned());
    }
    if !(1..=12).contains(&padding) {
        return Err("--padding must be between 1 and 12".to_owned());
    }
    let path = path.unwrap_or_else(|| PathBuf::from("."));
    let root = fs::canonicalize(&path)
        .map_err(|error| format!("cannot access {}: {error}", path.display()))?;
    if !root.is_dir() {
        return Err(format!("expected a directory: {}", root.display()));
    }
    if apply
        && (fs::symlink_metadata(root.join(ORGANIZE_MANIFEST_TEMP)).is_ok()
            || fs::symlink_metadata(root.join(ORGANIZE_MANIFEST)).is_ok())
    {
        return Err(format!(
            "an undo manifest or recovery file already exists under {}; run 'mediaforge undo {}' first",
            root.display(),
            root.display()
        ));
    }
    let ignored = ignored_by_git(&root)?;
    let mut entries = fs::read_dir(&root)
        .map_err(|error| format!("cannot list {}: {error}", root.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("cannot read directory entry: {error}"))?;
    if natural {
        let mut keyed = entries
            .drain(..)
            .map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                (name, entry)
            })
            .collect::<Vec<_>>();
        keyed.sort_unstable_by(|(left, _), (right, _)| {
            natural_name_cmp(left, right).then_with(|| left.cmp(right))
        });
        entries.extend(keyed.into_iter().map(|(_, entry)| entry));
    } else {
        sort_entries(&mut entries);
    }
    let sidecars_by_asset = collect_unity_meta_sidecars(&entries);
    let mut moves = Vec::<PlannedMove>::new();
    let mut skipped = 0_usize;
    let mut collision_examples = Vec::<(String, String)>::new();
    let mut collision_count = 0_usize;
    let mut index = 0_u64;
    for entry in entries {
        let source = entry.path();
        let name = entry.file_name();
        let Some(name_text) = name.to_str() else {
            skipped += 1;
            continue;
        };
        if (!include_hidden && is_dot_hidden(&source))
            || is_ignored(name_text, &ignored)
            || matches!(
                name_text,
                ".gitignore"
                    | ORGANIZE_MANIFEST
                    | ORGANIZE_MANIFEST_TEMP
                    | ORGANIZE_COMPLETE_MARKER
            )
        {
            skipped += 1;
            continue;
        }
        let metadata = fs::symlink_metadata(&source)
            .map_err(|error| format!("cannot inspect {}: {error}", source.display()))?;
        if !include_hidden && metadata_has_hidden_attribute(&metadata) {
            skipped += 1;
            continue;
        }
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            skipped += 1;
            continue;
        }
        if let Some(asset_name) = strip_ascii_case_insensitive_suffix(name_text, ".meta") {
            let asset_path = root.join(asset_name);
            match fs::symlink_metadata(&asset_path) {
                Ok(asset_metadata)
                    if !asset_metadata.file_type().is_symlink()
                        && (asset_metadata.is_file() || asset_metadata.is_dir()) =>
                {
                    if asset_metadata.is_dir() {
                        skipped += 1;
                    }
                    continue;
                }
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!(
                        "cannot inspect Unity asset {}: {error}",
                        asset_path.display()
                    ));
                }
            }
        }
        let extension = source.extension().and_then(|value| value.to_str());
        if extension_filter
            .as_ref()
            .is_some_and(|extensions| !matches_extension_filter(extension, extensions))
        {
            continue;
        }
        let sequence_number = start
            .checked_add(index)
            .ok_or_else(|| "sequence number overflow".to_owned())?;
        index = index
            .checked_add(1)
            .ok_or_else(|| "too many files to sequence".to_owned())?;
        let original_extension = source
            .extension()
            .and_then(|value| value.to_str())
            .map(|value| format!(".{value}"))
            .unwrap_or_default();
        let target_name = format!("{prefix}_{sequence_number:0padding$}{original_extension}");
        let from_text = portable_relative_path(Path::new(name_text));
        let to_text = portable_relative_path(Path::new(&target_name));
        let (Some(from_text), Some(to_text)) = (from_text, to_text) else {
            skipped += 1;
            continue;
        };
        let sidecar_name = match sidecars_by_asset.get(&unity_meta_asset_key(name_text)) {
            Some(Some(sidecar_name)) => sidecar_name.clone(),
            Some(None) => {
                skipped += 1;
                collision_count += 1;
                if collision_examples.len() < 100 {
                    collision_examples.push((from_text, "ambiguous_sidecar".to_owned()));
                }
                continue;
            }
            None => format!("{name_text}.meta"),
        };
        let sidecar = root.join(&sidecar_name);
        let sidecar_state = entry_state(&sidecar)
            .map_err(|error| format!("cannot inspect sidecar {}: {error}", sidecar.display()))?;
        let target = root.join(&target_name);
        let state = entry_state(&target)
            .map_err(|error| format!("cannot inspect target {}: {error}", target.display()))?;
        let sidecar_target_name = format!("{target_name}.meta");
        let sidecar_target = root.join(&sidecar_target_name);
        let sidecar_target_state = entry_state(&sidecar_target).map_err(|error| {
            format!(
                "cannot inspect sidecar target {}: {error}",
                sidecar_target.display()
            )
        })?;
        if from_text == to_text
            || state != EntryState::Missing
            || sidecar_state == EntryState::Other
            || sidecar_target_state != EntryState::Missing
            || target_name.encode_utf16().count() > 255
            || (sidecar_state == EntryState::File
                && sidecar_target_name.encode_utf16().count() > 255)
        {
            skipped += 1;
            collision_count += 1;
            if collision_examples.len() < 100 {
                collision_examples.push((
                    from_text,
                    if state == EntryState::Missing && sidecar_target_state == EntryState::Missing {
                        if sidecar_state == EntryState::Other {
                            "sidecar_not_regular".to_owned()
                        } else if target_name.encode_utf16().count() > 255
                            || (sidecar_state == EntryState::File
                                && sidecar_target_name.encode_utf16().count() > 255)
                        {
                            "filename_too_long".to_owned()
                        } else {
                            "already_named".to_owned()
                        }
                    } else if state != EntryState::Missing {
                        "destination_exists".to_owned()
                    } else if sidecar_target_state != EntryState::Missing {
                        "sidecar_destination_exists".to_owned()
                    } else {
                        "sidecar_not_regular".to_owned()
                    },
                ));
            }
            continue;
        }
        if sidecar_state == EntryState::File {
            let sidecar_from = portable_relative_path(Path::new(&sidecar_name));
            let sidecar_to = portable_relative_path(Path::new(&sidecar_target_name));
            let (Some(sidecar_from), Some(sidecar_to)) = (sidecar_from, sidecar_to) else {
                return Err(format!(
                    "cannot create a portable Unity sidecar path for {name_text}"
                ));
            };
            moves.push(PlannedMove {
                from: sidecar_from,
                to: sidecar_to,
            });
        }
        moves.push(PlannedMove {
            from: from_text,
            to: to_text,
        });
    }

    if json && !apply {
        write_json_stdout("sequence JSON", |output| {
            let mut output = JsonWhitespaceWriter::new(output, compact);
            if summary_only {
                return writeln!(
                    output,
                    "{{\"root\":{},\"prefix\":{},\"applied\":false,\"moves\":null,\"moves_included\":false,\"move_count\":{},\"skipped_count\":{},\"skipped_examples\":null,\"skipped_examples_included\":false,\"skipped_examples_truncated\":{}}}",
                    json_string(&root.to_string_lossy()),
                    json_string(&prefix),
                    moves.len(),
                    skipped,
                    collision_count > collision_examples.len()
                );
            }
            writeln!(
                output,
                "{{\n  \"root\": {},\n  \"prefix\": {},\n  \"applied\": false,\n  \"moves\": [",
                json_string(&root.to_string_lossy()),
                json_string(&prefix)
            )?;
            for (position, movement) in moves.iter().enumerate() {
                let comma = if position + 1 == moves.len() { "" } else { "," };
                writeln!(
                    output,
                    "    {{\"from\": {}, \"to\": {}}}{}",
                    json_string(&movement.from),
                    json_string(&movement.to),
                    comma
                )?;
            }
            writeln!(
                output,
                "  ],\n  \"move_count\": {},\n  \"skipped_count\": {},\n  \"skipped_examples\": [",
                moves.len(),
                skipped
            )?;
            for (position, (path, reason)) in collision_examples.iter().enumerate() {
                let comma = if position + 1 == collision_examples.len() {
                    ""
                } else {
                    ","
                };
                writeln!(
                    output,
                    "    {{\"path\": {}, \"reason\": {}}}{}",
                    json_string(path),
                    json_string(reason),
                    comma
                )?;
            }
            writeln!(
                output,
                "  ],\n  \"skipped_examples_truncated\": {}\n}}",
                collision_count > collision_examples.len()
            )
        })?;
    } else if !json {
        println!(
            "Sequence preview: {} file(s) to rename; {skipped} skipped",
            moves.len()
        );
        for movement in moves.iter().take(30) {
            println!("  {} -> {}", movement.from, movement.to);
        }
        if moves.len() > 30 {
            println!("  ... and {} more", moves.len() - 30);
        }
    }
    if !apply {
        if !moves.is_empty() && !json {
            println!(
                "\nPreview only. Re-run with --apply to rename files; use 'mediaforge undo {}' to restore them.",
                root.display()
            );
        }
        return Ok(());
    }
    if moves.is_empty() {
        if json {
            write_json_stdout("sequence JSON", |output| {
                let mut output = JsonWhitespaceWriter::new(output, compact);
                writeln!(
                    output,
                    "{{\"root\": {}, \"prefix\": {}, \"applied\": true, \"moves\": [], \"move_count\": 0, \"renamed_count\": 0, \"skipped_count\": {}, \"result\": \"no_changes\"}}",
                    json_string(&root.to_string_lossy()),
                    json_string(&prefix),
                    skipped
                )
            })?;
        } else {
            println!("No files need renaming.");
        }
        return Ok(());
    }
    for movement in &moves {
        let from = validate_relative_path(&movement.from)?;
        let to = validate_relative_path(&movement.to)?;
        reject_symlink_components(&root, &from)?;
        reject_symlink_components(&root, &to)?;
        if entry_state(&root.join(&from))
            .map_err(|error| format!("cannot inspect source {}: {error}", from.display()))?
            != EntryState::File
            || entry_state(&root.join(&to))
                .map_err(|error| format!("cannot inspect target {}: {error}", to.display()))?
                != EntryState::Missing
        {
            return Err(format!(
                "file plan changed since preview; refusing to apply: {}",
                movement.from
            ));
        }
    }
    write_organize_manifest(&root, &moves)?;
    let mut renamed = 0_usize;
    for movement in &moves {
        let from = root.join(validate_relative_path(&movement.from)?);
        let to = root.join(validate_relative_path(&movement.to)?);
        if let Err(error) = move_file_no_replace(&from, &to) {
            return Err(format!(
                "renamed {renamed} file(s) before failure: {error}; recovery manifest remains at {}",
                root.join(ORGANIZE_MANIFEST).display()
            ));
        }
        renamed += 1;
    }
    write_organize_complete_marker(&root)?;
    if json {
        write_json_stdout("sequence JSON", |output| {
            let mut output = JsonWhitespaceWriter::new(output, compact);
            if summary_only {
                return writeln!(
                    output,
                    "{{\"root\":{},\"prefix\":{},\"applied\":true,\"moves\":null,\"moves_included\":false,\"move_count\":{},\"renamed_count\":{renamed},\"skipped_count\":{},\"skipped_examples\":null,\"skipped_examples_included\":false,\"result\":\"applied\",\"undo_manifest\":{}}}",
                    json_string(&root.to_string_lossy()),
                    json_string(&prefix),
                    moves.len(),
                    skipped,
                    json_string(&root.join(ORGANIZE_MANIFEST).to_string_lossy())
                );
            }
            writeln!(
                output,
                "{{\n  \"root\": {},\n  \"prefix\": {},\n  \"applied\": true,\n  \"moves\": [",
                json_string(&root.to_string_lossy()),
                json_string(&prefix)
            )?;
            for (position, movement) in moves.iter().enumerate() {
                let comma = if position + 1 == moves.len() { "" } else { "," };
                writeln!(
                    output,
                    "    {{\"from\": {}, \"to\": {}}}{}",
                    json_string(&movement.from),
                    json_string(&movement.to),
                    comma
                )?;
            }
            writeln!(
                output,
                "  ],\n  \"move_count\": {},\n  \"renamed_count\": {},\n  \"skipped_count\": {},\n  \"result\": \"applied\",\n  \"undo_manifest\": {}\n}}",
                moves.len(),
                renamed,
                skipped,
                json_string(&root.join(ORGANIZE_MANIFEST).to_string_lossy())
            )
        })?;
    } else {
        println!(
            "Renamed {renamed} file(s). Undo manifest: {}",
            root.join(ORGANIZE_MANIFEST).display()
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
    if field.len() < 2 {
        return Err("tar numeric field overflow".to_owned());
    }
    field.fill(b'0');
    field[field.len() - 1] = 0;
    let mut remaining = value;
    let mut cursor = field.len() - 1;
    loop {
        if cursor == 0 {
            return Err("tar numeric field overflow".to_owned());
        }
        cursor -= 1;
        field[cursor] = b'0' + (remaining & 0o7) as u8;
        remaining >>= 3;
        if remaining == 0 {
            break;
        }
    }
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
    let mut checksum_field = [b'0'; 8];
    checksum_field[6] = 0;
    checksum_field[7] = b' ';
    let mut remaining = checksum;
    for digit in checksum_field[..6].iter_mut().rev() {
        *digit = b'0' + (remaining & 0o7) as u8;
        remaining >>= 3;
    }
    header[148..156].copy_from_slice(&checksum_field);
    Ok(header)
}

fn write_tar_payload(writer: &mut impl Write, contents: &[u8]) -> io::Result<()> {
    writer.write_all(contents)?;
    let padding = (TAR_BLOCK_SIZE - contents.len() % TAR_BLOCK_SIZE) % TAR_BLOCK_SIZE;
    if padding > 0 {
        writer.write_all(&TAR_ZERO_BLOCK[..padding])?;
    }
    Ok(())
}

struct PackFileRecord {
    relative: String,
    size: u64,
    modified: Option<std::time::SystemTime>,
    kind: Kind,
}

fn pack_relative_path(path: &Path) -> Result<String, String> {
    let mut relative = String::with_capacity(path.as_os_str().len());
    for component in path.components() {
        let std::path::Component::Normal(name) = component else {
            return Err(format!("unsafe archive path: {}", path.display()));
        };
        let name = name
            .to_str()
            .ok_or_else(|| format!("non-UTF-8 path cannot be packed: {}", path.display()))?;
        if !relative.is_empty() {
            relative.push('/');
        }
        relative.push_str(name);
    }
    Ok(relative)
}

fn pack_file(
    writer: &mut impl Write,
    archive_name: &str,
    record: &PackFileRecord,
    source_path: &Path,
    index: usize,
    copy_buffer: &mut [u8],
) -> Result<(), String> {
    let path_bytes = archive_name.as_bytes();
    let split = ustar_name(path_bytes);
    let pax_path = split.is_none();
    let source_metadata = fs::symlink_metadata(source_path)
        .map_err(|error| format!("cannot inspect {}: {error}", source_path.display()))?;
    if !source_metadata.is_file() || source_metadata.file_type().is_symlink() {
        return Err(format!(
            "source changed type since scan: {}",
            source_path.display()
        ));
    }
    let mut input = File::open(source_path)
        .map_err(|error| format!("cannot open {}: {error}", source_path.display()))?;
    let initial_metadata = input.metadata().map_err(|error| {
        format!(
            "could not inspect {} before archiving: {error}",
            source_path.display()
        )
    })?;
    if !initial_metadata.is_file()
        || initial_metadata.len() != record.size
        || initial_metadata.modified().ok() != record.modified
    {
        return Err(format!(
            "source changed before packing: {}",
            source_path.display()
        ));
    }
    let source_modified = source_metadata.modified().ok();
    if source_metadata.len() != record.size || source_modified != record.modified {
        return Err(format!(
            "source changed since scan: {}",
            source_path.display()
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
            append_pax_record(&mut pax, "path", archive_name)?;
        }
        if pax_size {
            append_pax_record(&mut pax, "size", &record.size.to_string())?;
        }
        if pax_mtime {
            append_pax_record(&mut pax, "mtime", &modified.to_string())?;
        }
        let pax_name = format!("PaxHeaders/MediaForge/{index}");
        let header = tar_header(pax_name.as_bytes(), &[], 0o644, pax.len() as u64, 0, b'x')?;
        writer
            .write_all(&header)
            .and_then(|()| write_tar_payload(writer, pax.as_bytes()))
            .map_err(|error| format!("could not write PAX header: {error}"))?;
    }

    let fallback_name = pax_path.then(|| format!("PaxFiles/{index}"));
    let (header_name, prefix) = match split {
        Some((name, prefix)) => (name, prefix),
        None => (
            fallback_name
                .as_deref()
                .expect("PAX fallback name exists for long paths")
                .as_bytes(),
            &[][..],
        ),
    };
    let header_size = if pax_size { 0 } else { record.size };
    let header_mtime = modified.min(TAR_OCTAL_MAX);
    let header = tar_header(header_name, prefix, mode, header_size, header_mtime, b'0')?;
    writer
        .write_all(&header)
        .map_err(|error| format!("could not write tar header: {error}"))?;

    let mut remaining = record.size;
    while remaining > 0 {
        let requested = remaining.min(copy_buffer.len() as u64) as usize;
        let count = read_retry_interrupted(&mut input, &mut copy_buffer[..requested])
            .map_err(|error| format!("could not read {}: {error}", source_path.display()))?;
        if count == 0 {
            break;
        }
        writer
            .write_all(&copy_buffer[..count])
            .map_err(|error| format!("could not archive {}: {error}", source_path.display()))?;
        remaining -= count as u64;
    }
    let copied = record.size - remaining;
    if copied != record.size {
        return Err(format!(
            "source changed while packing: {}",
            source_path.display()
        ));
    }
    let mut extra = [0_u8; 1];
    if read_retry_interrupted(&mut input, &mut extra)
        .map_err(|error| format!("could not verify {}: {error}", source_path.display()))?
        != 0
    {
        return Err(format!(
            "source grew while packing: {}",
            source_path.display()
        ));
    }
    let final_metadata = input
        .metadata()
        .map_err(|error| format!("could not verify {}: {error}", source_path.display()))?;
    if final_metadata.len() != record.size || final_metadata.modified().ok() != record.modified {
        return Err(format!(
            "source changed while packing: {}",
            source_path.display()
        ));
    }
    let padding =
        (TAR_BLOCK_SIZE as u64 - record.size % TAR_BLOCK_SIZE as u64) % TAR_BLOCK_SIZE as u64;
    if padding > 0 {
        writer
            .write_all(&TAR_ZERO_BLOCK[..padding as usize])
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
        let kind = parse_asset_kind(name).ok_or_else(|| {
            format!(
                "unknown pack type '{name}'; choose video, audio, image, model, texture, project, or other"
            )
        })?;
        kinds.insert(kind);
    }
    Ok(kinds)
}

fn parse_extensions(value: &str) -> Result<HashSet<String>, String> {
    let mut extensions = HashSet::new();
    for name in value.split(',').map(str::trim) {
        if name.is_empty() {
            return Err("--extension contains an empty value".to_owned());
        }
        if name.eq_ignore_ascii_case("none") {
            extensions.insert("[no extension]".to_owned());
            continue;
        }
        let extension = name.strip_prefix('.').unwrap_or(name);
        if extension.is_empty()
            || extension.starts_with('.')
            || !extension
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(format!("invalid file extension '{name}'"));
        }
        extensions.insert(format!(".{}", extension.to_ascii_lowercase()));
    }
    Ok(extensions)
}

fn validate_archive_component(component: &str) -> Result<(), String> {
    if component.is_empty()
        || component.ends_with('.')
        || component.ends_with(' ')
        || component
            .chars()
            .any(|character| character <= '\u{1f}' || "<>\"|?*:\\/".contains(character))
    {
        return Err(format!(
            "path component is not portable to Windows: {component:?}"
        ));
    }
    let device_name = component
        .split('.')
        .next()
        .unwrap_or(component)
        .trim_end_matches([' ', '.']);
    let reserved = ["CON", "PRN", "AUX", "NUL"]
        .iter()
        .any(|reserved| device_name.eq_ignore_ascii_case(reserved))
        || ["COM", "LPT"].iter().any(|prefix| {
            let has_prefix = device_name
                .get(..prefix.len())
                .is_some_and(|value| value.eq_ignore_ascii_case(prefix));
            has_prefix
                && device_name.get(prefix.len()..).is_some_and(|suffix| {
                    matches!(suffix, "¹" | "²" | "³")
                        || (suffix.len() == 1 && matches!(suffix.as_bytes()[0], b'1'..=b'9'))
                })
        });
    if reserved {
        return Err(format!(
            "path component is a reserved Windows device name: {component:?}"
        ));
    }
    Ok(())
}

fn register_portable_archive_path(
    paths: &mut HashMap<String, (usize, usize, bool)>,
    records: &[PackFileRecord],
    record_index: usize,
    relative: &Path,
    display_name: &str,
    portable_prefix: &mut String,
) -> Result<(), String> {
    let mut components = relative.components().peekable();
    let mut actual_prefix_len = 0;
    portable_prefix.clear();
    while let Some(component) = components.next() {
        let std::path::Component::Normal(component) = component else {
            return Err(format!("unsafe archive path: {}", relative.display()));
        };
        let component = component
            .to_str()
            .ok_or_else(|| format!("non-UTF-8 path cannot be packed: {}", relative.display()))?;
        validate_archive_component(component)?;
        if !portable_prefix.is_empty() {
            actual_prefix_len += 1;
            portable_prefix.push('/');
        }
        actual_prefix_len += component.len();
        portable_prefix.extend(component.chars().flat_map(char::to_lowercase));
        let is_file = components.peek().is_none();
        match paths.get(portable_prefix.as_str()) {
            Some((existing_index, existing_len, true)) if !is_file => {
                return Err(format!(
                    "archive path uses a file as a directory on case-insensitive filesystems: {:?} and {display_name:?}",
                    &records[*existing_index].relative[..*existing_len]
                ));
            }
            Some((existing_index, existing_len, false)) if is_file => {
                return Err(format!(
                    "archive path uses a directory as a file on case-insensitive filesystems: {:?} and {display_name:?}",
                    &records[*existing_index].relative[..*existing_len]
                ));
            }
            Some((existing_index, existing_len, true)) if is_file => {
                return Err(format!(
                    "archive paths collide on case-insensitive filesystems: {:?} and {display_name:?}",
                    &records[*existing_index].relative[..*existing_len]
                ));
            }
            Some(_) => {}
            None => {
                paths.insert(
                    portable_prefix.clone(),
                    (record_index, actual_prefix_len, is_file),
                );
            }
        }
    }
    Ok(())
}

fn padded_tar_size(size: u64) -> Result<u64, String> {
    size.checked_add(TAR_BLOCK_SIZE as u64 - 1)
        .map(|size| size / TAR_BLOCK_SIZE as u64 * TAR_BLOCK_SIZE as u64)
        .ok_or_else(|| "estimated archive size exceeds the supported range".to_owned())
}

fn decimal_digits_usize(mut value: usize) -> usize {
    let mut digits = 1;
    while value >= 10 {
        value /= 10;
        digits += 1;
    }
    digits
}

fn decimal_digits_u64(mut value: u64) -> usize {
    let mut digits = 1;
    while value >= 10 {
        value /= 10;
        digits += 1;
    }
    digits
}

fn pax_record_encoded_len(key_len: usize, value_len: usize) -> Option<usize> {
    let body_len = key_len.checked_add(value_len)?.checked_add(3)?;
    let mut record_len = body_len.checked_add(1)?;
    loop {
        let updated_len = body_len.checked_add(decimal_digits_usize(record_len))?;
        if updated_len == record_len {
            return Some(record_len);
        }
        record_len = updated_len;
    }
}

fn append_pax_record(output: &mut String, key: &str, value: &str) -> Result<(), String> {
    let record_len = pax_record_encoded_len(key.len(), value.len())
        .ok_or_else(|| "PAX header size exceeds the supported range".to_owned())?;
    output
        .try_reserve(record_len)
        .map_err(|_| "PAX header exceeds available memory".to_owned())?;
    writeln!(output, "{record_len} {key}={value}").expect("writing a PAX record into a String");
    Ok(())
}

fn estimate_tar_size(entries: &[PackFileRecord]) -> Result<u64, String> {
    let mut total = (TAR_BLOCK_SIZE * 2) as u64;
    for record in entries {
        let archive_name = &record.relative;
        let pax_path = ustar_name(archive_name.as_bytes()).is_none();
        let modified = record
            .modified
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |duration| duration.as_secs());
        let needs_pax_size = record.size > TAR_OCTAL_MAX;
        let pax_mtime = modified > TAR_OCTAL_MAX;

        if pax_path || needs_pax_size || pax_mtime {
            let mut pax_bytes = 0_usize;
            if pax_path {
                pax_bytes = pax_bytes
                    .checked_add(
                        pax_record_encoded_len("path".len(), archive_name.len()).ok_or_else(
                            || "PAX header size exceeds the supported range".to_owned(),
                        )?,
                    )
                    .ok_or_else(|| "PAX header size exceeds the supported range".to_owned())?;
            }
            if needs_pax_size {
                pax_bytes = pax_bytes
                    .checked_add(
                        pax_record_encoded_len("size".len(), decimal_digits_u64(record.size))
                            .ok_or_else(|| {
                                "PAX header size exceeds the supported range".to_owned()
                            })?,
                    )
                    .ok_or_else(|| "PAX header size exceeds the supported range".to_owned())?;
            }
            if pax_mtime {
                pax_bytes = pax_bytes
                    .checked_add(
                        pax_record_encoded_len("mtime".len(), decimal_digits_u64(modified))
                            .ok_or_else(|| {
                                "PAX header size exceeds the supported range".to_owned()
                            })?,
                    )
                    .ok_or_else(|| "PAX header size exceeds the supported range".to_owned())?;
            }
            let pax_size = u64::try_from(pax_bytes)
                .map_err(|_| "PAX header size exceeds the supported range".to_owned())?;
            total = total
                .checked_add(TAR_BLOCK_SIZE as u64)
                .and_then(|total| padded_tar_size(pax_size).ok()?.checked_add(total))
                .ok_or_else(|| "estimated archive size exceeds the supported range".to_owned())?;
        }

        total = total
            .checked_add(TAR_BLOCK_SIZE as u64)
            .and_then(|total| padded_tar_size(record.size).ok()?.checked_add(total))
            .ok_or_else(|| "estimated archive size exceeds the supported range".to_owned())?;
    }
    Ok(total)
}

struct PackJsonOptions<'a> {
    root: &'a Path,
    output: &'a Path,
    status: &'a str,
    dry_run: bool,
    output_exists: bool,
    min_size: u64,
    max_size: Option<u64>,
    exclude_path_contains: &'a [String],
    files: usize,
    input_bytes: u64,
    estimated_archive_bytes: u64,
    list_entries: bool,
    compact: bool,
    entries: &'a [PackFileRecord],
    archive_bytes: Option<u64>,
}

fn print_pack_json(options: PackJsonOptions<'_>) -> Result<(), String> {
    let PackJsonOptions {
        root,
        output,
        status,
        dry_run,
        output_exists,
        min_size,
        max_size,
        exclude_path_contains,
        files,
        input_bytes,
        estimated_archive_bytes,
        list_entries,
        compact,
        entries,
        archive_bytes,
    } = options;
    let stdout = io::stdout();
    let output_buffer = io::BufWriter::with_capacity(64 * 1024, stdout.lock());
    let mut writer = JsonWhitespaceWriter::new(output_buffer, compact);
    let result = (|| -> io::Result<()> {
        writer.write_all(b"{\n  \"root\": ")?;
        write_json_string(&mut writer, &root.to_string_lossy())?;
        writer.write_all(b",\n  \"output\": ")?;
        write_json_string(&mut writer, &output.to_string_lossy())?;
        writer.write_all(b",\n  \"status\": ")?;
        write_json_string(&mut writer, status)?;
        writeln!(
            writer,
            ",\n  \"dry_run\": {dry_run},\n  \"output_exists\": {output_exists},\n  \"min_size_bytes\": {min_size},"
        )?;
        match max_size {
            Some(size) => writeln!(writer, "  \"max_size_bytes\": {size},")?,
            None => writer.write_all(b"  \"max_size_bytes\": null,\n")?,
        }
        writer.write_all(b"  \"exclude_path_contains\": [")?;
        for (index, value) in exclude_path_contains.iter().enumerate() {
            if index > 0 {
                writer.write_all(b",")?;
            }
            write_json_string(&mut writer, value)?;
        }
        writeln!(
            writer,
            "],\n  \"files\": {files},\n  \"input_bytes\": {input_bytes},\n  \"estimated_archive_bytes\": {estimated_archive_bytes},"
        )?;
        writer.write_all(b"  \"input_size\": ")?;
        write_json_string(&mut writer, &human_size(input_bytes))?;
        writer.write_all(b",\n")?;
        match archive_bytes {
            Some(bytes) => {
                writeln!(writer, "  \"archive_bytes\": {bytes},")?;
                writer.write_all(b"  \"archive_size\": ")?;
                write_json_string(&mut writer, &human_size(bytes))?;
                writer.write_all(b",\n")?;
            }
            None => writer.write_all(b"  \"archive_bytes\": null,\n  \"archive_size\": null,\n")?,
        }
        if list_entries {
            writer.write_all(b"  \"entries\": [\n")?;
            for (index, record) in entries.iter().enumerate() {
                if index > 0 {
                    writer.write_all(b",\n")?;
                }
                writer.write_all(b"    {\"path\": ")?;
                write_json_string(&mut writer, &record.relative)?;
                write!(writer, ", \"size_bytes\": {}, \"type\": ", record.size)?;
                write_json_string(&mut writer, record.kind.as_str())?;
                writer.write_all(b"}")?;
            }
            writer.write_all(b"\n  ]\n}\n")?;
        } else {
            writer.write_all(b"  \"entries\": null\n}\n")?;
        }
        writer.flush()
    })();
    result.map_err(|error| format!("could not write pack JSON: {error}"))
}

fn run_pack(args: &[String]) -> Result<(), String> {
    let mut path = None;
    let mut output = None;
    let mut include_hidden = false;
    let mut include_generated = false;
    let mut dry_run = false;
    let mut json = false;
    let mut compact = false;
    let mut list_entries = false;
    let mut selected_types: Option<HashSet<Kind>> = None;
    let mut selected_extensions: Option<HashSet<String>> = None;
    let mut exclude_path_contains = Vec::<String>::new();
    let mut min_size = 0_u64;
    let mut max_size = None;
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
            "--json" => json = true,
            "--compact" => compact = true,
            "--list" => list_entries = true,
            "--type" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--type requires a comma-separated type list".to_owned())?;
                selected_types = Some(parse_pack_types(value)?);
            }
            "--extension" => {
                let value = iter.next().ok_or_else(|| {
                    "--extension requires a comma-separated extension list".to_owned()
                })?;
                selected_extensions = Some(parse_extensions(value)?);
            }
            "--exclude-path-contains" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--exclude-path-contains requires a substring".to_owned())?;
                if value.is_empty() {
                    return Err("--exclude-path-contains cannot be empty".to_owned());
                }
                exclude_path_contains.push(value.clone());
            }
            "--min-size" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--min-size requires a value".to_owned())?;
                min_size = parse_size(value)?;
            }
            "--max-size" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--max-size requires a value".to_owned())?;
                max_size = Some(parse_size(value)?);
            }
            "--help" | "-h" => {
                println!(
                    "Usage: mediaforge pack <PATH> --output <FILE.tar> [--type TYPES] [--extension EXTENSIONS] [--min-size SIZE] [--max-size SIZE] [--exclude-path-contains TEXT ...] [--include-hidden] [--include-generated] [--dry-run] [--json] [--compact] [--list]\n\nCreate an uncompressed, stream-written tar archive. --type accepts comma-separated scan types: video, audio, image, model, texture, project, other. --extension accepts comma-separated extensions such as mov,mxf; use none for files without an extension. Matching Unity .meta sidecars are included with selected assets unless ignored, hidden, or explicitly excluded. --min-size and --max-size accept bytes or K/M/G/T binary units. Repeat --exclude-path-contains to omit matching relative paths; matching directory names prune their subtrees. Git-ignored and hidden files are skipped by default. Common Unity, Unreal, and Godot generated folders are excluded unless --include-generated is used. PAX extensions support long paths and files larger than 8 GiB. --json emits a machine-readable result summary; --list includes per-file paths, sizes, and types in text or JSON output. --compact removes formatting whitespace from JSON output and requires --json."
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
    if compact && !json {
        return Err("--compact can only be used with --json".to_owned());
    }
    let path = path.ok_or_else(|| "pack requires an input directory".to_owned())?;
    let output = output.ok_or_else(|| "pack requires --output <FILE.tar>".to_owned())?;
    if max_size.is_some_and(|max_size| max_size < min_size) {
        return Err("--max-size must be greater than or equal to --min-size".to_owned());
    }
    let root = fs::canonicalize(&path)
        .map_err(|error| format!("cannot access {}: {error}", path.display()))?;
    if !root.is_dir() {
        return Err(format!("expected a project directory: {}", root.display()));
    }
    let requested_name = output
        .file_name()
        .ok_or_else(|| "output must name a file".to_owned())?;
    let requested_name_text = requested_name
        .to_str()
        .ok_or_else(|| "archive output filename must be valid UTF-8".to_owned())?;
    validate_archive_component(requested_name_text)
        .map_err(|error| format!("invalid archive output filename: {error}"))?;
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
    let output_exists = entry_state(&output_path)
        .map_err(|error| format!("cannot inspect output {}: {error}", output_path.display()))?
        != EntryState::Missing;
    if output_exists && !dry_run {
        return Err(format!(
            "output already exists; refusing to overwrite: {}",
            output_path.display()
        ));
    }

    let ignored = ignored_by_git(&root)?;
    let mut records = Vec::<PackFileRecord>::new();
    let mut skip_dirs = SKIP_DIRS.to_vec();
    if !include_generated {
        skip_dirs.extend_from_slice(PACK_GENERATED_DIRS);
    }
    let exclude_path_contains_lower = exclude_path_contains
        .iter()
        .map(|value| value.to_lowercase())
        .collect::<Vec<_>>();
    let kinds = selected_types
        .as_ref()
        .map(|types| types.iter().copied().collect::<Vec<_>>());
    let filters = StrictWalkFilters {
        kinds: kinds.as_deref(),
        extensions: selected_extensions.as_ref(),
        sort_entries_per_directory: false,
        min_size: Some(min_size),
        max_size,
        excluded_subtree: None,
        exclude_path_contains_lower: Some(&exclude_path_contains_lower),
        omit_absolute_path: false,
        store_relative_path: true,
        omit_modified_time: false,
        omit_extension: true,
    };
    walk_strict_filtered_with_callback(
        &root,
        &root,
        include_hidden,
        &skip_dirs,
        &ignored,
        &filters,
        |record| {
            let relative = pack_relative_path(&record.path)?;
            records.push(PackFileRecord {
                relative,
                size: record.size,
                modified: record.modified,
                kind: record.kind,
            });
            Ok(())
        },
    )?;
    let selected_path_keys = records
        .iter()
        .filter(|record| {
            Path::new(&record.relative)
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| strip_ascii_case_insensitive_suffix(name, ".meta").is_some())
        })
        .map(|record| output_path_collision_key(Path::new(&record.relative)))
        .collect::<HashSet<_>>();
    records.sort_unstable_by(|left, right| {
        Path::new(&left.relative)
            .parent()
            .cmp(&Path::new(&right.relative).parent())
            .then_with(|| left.relative.cmp(&right.relative))
    });
    let mut ignore_rule_chain = vec![(PathBuf::new(), Arc::<[IgnoreRule]>::from(ignored.clone()))];
    let mut unity_sidecars = Vec::<PackFileRecord>::new();
    let mut group_start = 0;
    while group_start < records.len() {
        let parent = Path::new(&records[group_start].relative)
            .parent()
            .unwrap_or(Path::new("."))
            .to_path_buf();
        let mut group_end = group_start + 1;
        while group_end < records.len()
            && Path::new(&records[group_end].relative)
                .parent()
                .unwrap_or(Path::new("."))
                == parent
        {
            group_end += 1;
        }
        let directory_records = &records[group_start..group_end];
        if directory_records.iter().any(|record| {
            Path::new(&record.relative)
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| strip_ascii_case_insensitive_suffix(name, ".meta").is_none())
        }) {
            let directory = root.join(&parent);
            let entries = fs::read_dir(&directory)
                .map_err(|error| format!("cannot read directory {}: {error}", directory.display()))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| {
                    format!("cannot read directory {}: {error}", directory.display())
                })?;
            let sidecars_by_name = collect_unity_meta_sidecars(&entries);
            for record in directory_records {
                let Some(asset_name) = Path::new(&record.relative)
                    .file_name()
                    .and_then(|name| name.to_str())
                else {
                    continue;
                };
                if strip_ascii_case_insensitive_suffix(asset_name, ".meta").is_some() {
                    continue;
                }
                let sidecar_name = match sidecars_by_name.get(&unity_meta_asset_key(asset_name)) {
                    Some(Some(name)) => name,
                    Some(None) => {
                        return Err(format!(
                            "ambiguous Unity .meta sidecars for {}",
                            record.relative
                        ));
                    }
                    None => continue,
                };
                let relative_path = parent.join(sidecar_name);
                let path_key = output_path_collision_key(&relative_path);
                if selected_path_keys.contains(&path_key) {
                    continue;
                }
                let relative = pack_relative_path(&relative_path)?;
                let relative_lower = relative.to_lowercase();
                if exclude_path_contains_lower
                    .iter()
                    .any(|value| relative_lower.contains(value))
                {
                    continue;
                }
                while !parent.starts_with(&ignore_rule_chain.last().unwrap().0) {
                    ignore_rule_chain.pop();
                }
                let mut walked_parent = ignore_rule_chain.last().unwrap().0.clone();
                let inherited_suffix = parent.strip_prefix(&walked_parent).map_err(|error| {
                    format!(
                        "cannot resolve ignore rules for {}: {error}",
                        parent.display()
                    )
                })?;
                for component in inherited_suffix.components() {
                    let std::path::Component::Normal(name) = component else {
                        continue;
                    };
                    walked_parent.push(name);
                    let inherited = &ignore_rule_chain.last().unwrap().1;
                    let directory = root.join(&walked_parent);
                    let rules = match extend_ignore_rules(&root, &directory, inherited)? {
                        Cow::Borrowed(_) => Arc::clone(inherited),
                        Cow::Owned(rules) => Arc::from(rules),
                    };
                    ignore_rule_chain.push((walked_parent.clone(), rules));
                }
                let active_ignored = &ignore_rule_chain.last().unwrap().1;
                if is_ignored(&relative, active_ignored) {
                    continue;
                }
                let sidecar_path = root.join(&relative_path);
                let metadata = fs::symlink_metadata(&sidecar_path).map_err(|error| {
                    format!("cannot inspect {}: {error}", sidecar_path.display())
                })?;
                if !metadata.is_file()
                    || metadata.file_type().is_symlink()
                    || (!include_hidden && metadata_has_hidden_attribute(&metadata))
                {
                    continue;
                }
                unity_sidecars.push(PackFileRecord {
                    relative,
                    size: metadata.len(),
                    modified: metadata.modified().ok(),
                    kind: Kind::Other,
                });
            }
        }
        group_start = group_end;
    }
    records.extend(unity_sidecars);
    let mut portable_names = HashMap::<String, (usize, usize, bool)>::with_capacity(records.len());
    let mut portable_prefix = String::new();
    let mut total_bytes = 0_u64;
    for (record_index, record) in records.iter().enumerate() {
        let relative = Path::new(&record.relative);
        let mut component_count = 0;
        for component in relative.components() {
            let std::path::Component::Normal(part) = component else {
                return Err(format!("unsafe archive path: {}", relative.display()));
            };
            let part = part.to_str().ok_or_else(|| {
                format!("non-UTF-8 path cannot be packed: {}", relative.display())
            })?;
            validate_archive_component(part)?;
            component_count += 1;
        }
        if component_count == 0 {
            return Err("cannot archive the input directory itself".to_owned());
        }
        if record.relative.len() > 1024 * 1024 {
            return Err(format!("archive path is too long: {}", record.relative));
        }
        register_portable_archive_path(
            &mut portable_names,
            &records,
            record_index,
            relative,
            &record.relative,
            &mut portable_prefix,
        )?;
        total_bytes = total_bytes
            .checked_add(record.size)
            .ok_or_else(|| "total input size exceeds the supported range".to_owned())?;
    }
    records.sort_unstable_by(|left, right| left.relative.cmp(&right.relative));
    let estimated_archive_bytes = estimate_tar_size(&records)?;
    if !json {
        println!(
            "Pack plan: {} file(s), {} input · estimated archive {}",
            records.len(),
            human_size(total_bytes),
            human_size(estimated_archive_bytes)
        );
        let displayed_entries = if list_entries {
            records.len()
        } else {
            records.len().min(30)
        };
        for record in records.iter().take(displayed_entries) {
            println!("  {}  {}", human_size(record.size), record.relative);
        }
        if records.len() > displayed_entries {
            println!("  ... and {} more", records.len() - displayed_entries);
        }
    }
    if dry_run {
        if json {
            print_pack_json(PackJsonOptions {
                root: &root,
                output: &output_path,
                status: if output_exists {
                    "output_exists"
                } else {
                    "planned"
                },
                dry_run: true,
                output_exists,
                min_size,
                max_size,
                exclude_path_contains: &exclude_path_contains,
                files: records.len(),
                input_bytes: total_bytes,
                estimated_archive_bytes,
                list_entries,
                compact,
                entries: &records,
                archive_bytes: None,
            })?;
            return Ok(());
        }
        if output_exists {
            println!(
                "Dry run; no archive was written. Output already exists and would not be overwritten."
            );
        } else {
            println!("Dry run; would create {}", output_path.display());
        }
        return Ok(());
    }

    let (temp_path, archive_file) =
        create_unique_temp_file(&output_parent, ".mediaforge-pack", "archive")
            .map_err(|error| format!("{error} beside {}", output_path.display()))?;
    let mut archive = BufWriter::with_capacity(64 * 1024, archive_file);
    let mut copy_buffer = vec![0_u8; 1024 * 1024];
    let mut archive_bytes = 0_u64;
    let write_result = (|| -> Result<(), String> {
        for (index, record) in records.iter().enumerate() {
            let source_path = root.join(&record.relative);
            pack_file(
                &mut archive,
                &record.relative,
                record,
                &source_path,
                index + 1,
                &mut copy_buffer,
            )?;
        }
        archive
            .write_all(&TAR_END_MARKER)
            .map_err(|error| format!("could not finish tar archive: {error}"))?;
        archive
            .flush()
            .map_err(|error| format!("could not flush archive: {error}"))?;
        archive
            .get_ref()
            .sync_all()
            .map_err(|error| format!("could not persist archive: {error}"))?;
        archive_bytes = archive
            .get_ref()
            .metadata()
            .map_err(|error| format!("could not inspect completed archive: {error}"))?
            .len();
        if archive_bytes != estimated_archive_bytes {
            return Err(format!(
                "archive size mismatch: wrote {archive_bytes} bytes, estimated {estimated_archive_bytes}"
            ));
        }
        Ok(())
    })();
    drop(archive);
    if let Err(error) = write_result {
        return Err(remove_temp_with_context(&temp_path, error));
    }
    if let Err(error) = move_file_no_replace(&temp_path, &output_path) {
        return Err(remove_temp_with_context(
            &temp_path,
            format!(
                "could not finalize archive {}: {error}",
                output_path.display()
            ),
        ));
    }
    let archive_size = archive_bytes;
    if json {
        print_pack_json(PackJsonOptions {
            root: &root,
            output: &output_path,
            status: "created",
            dry_run: false,
            output_exists: true,
            min_size,
            max_size,
            exclude_path_contains: &exclude_path_contains,
            files: records.len(),
            input_bytes: total_bytes,
            estimated_archive_bytes,
            list_entries,
            compact,
            entries: &records,
            archive_bytes: Some(archive_size),
        })?;
    } else {
        println!(
            "Created {} ({})",
            output_path.display(),
            human_size(archive_size)
        );
    }
    Ok(())
}

fn tool_version(executable: &str) -> Result<String, String> {
    let output = Command::new(executable)
        .arg("-version")
        .output()
        .map_err(|error| format!("unavailable: {error}"))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        let detail = detail.lines().next().unwrap_or("command failed").trim();
        return Err(format!("failed: {detail}"));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .lines()
        .next()
        .unwrap_or("version not reported")
        .to_owned())
}

fn ffmpeg_encoders() -> Result<HashSet<String>, String> {
    let output = Command::new("ffmpeg")
        .args(["-hide_banner", "-encoders"])
        .output()
        .map_err(|error| format!("unavailable: {error}"))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "failed: {}",
            detail.lines().next().unwrap_or("command failed").trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let flags = fields.next()?;
            let name = fields.next()?;
            (flags.len() == 6
                && flags
                    .chars()
                    .all(|flag| flag.is_ascii_alphanumeric() || flag == '.'))
            .then(|| name.to_owned())
        })
        .collect())
}

fn run_doctor(args: &[String]) -> Result<(), String> {
    let mut json = false;
    let mut strict = false;
    for arg in args {
        match arg.as_str() {
            "--json" => json = true,
            "--strict" => strict = true,
            "--help" | "-h" => {
                println!(
                    "Usage: mediaforge doctor [--json] [--strict]\n\nCheck whether ffmpeg and ffprobe are available on PATH, and whether FFmpeg includes the H.264/AAC, MP3, WAV, and FLAC encoders used by MediaForge. --strict returns a nonzero exit status if any check fails."
                );
                return Ok(());
            }
            value => return Err(format!("unknown doctor option: {value}")),
        }
    }
    let ffmpeg = tool_version("ffmpeg");
    let ffprobe = tool_version("ffprobe");
    let ready = ffmpeg.is_ok() && ffprobe.is_ok();
    let encoders = ffmpeg_encoders();
    let has_encoder = |name: &str| encoders.as_ref().is_ok_and(|set| set.contains(name));
    let proxy_ready = ffmpeg.is_ok() && has_encoder("libx264") && has_encoder("aac");
    let audio_ready = ffmpeg.is_ok()
        && has_encoder("libmp3lame")
        && has_encoder("pcm_s16le")
        && has_encoder("flac");
    let all_ready = ready && proxy_ready && audio_ready;
    if json {
        let tool_json = |result: &Result<String, String>| match result {
            Ok(version) => format!(
                "{{\"available\":true,\"version\":{}}}",
                json_string(version)
            ),
            Err(error) => format!("{{\"available\":false,\"error\":{}}}", json_string(error)),
        };
        write_json_stdout("doctor JSON", |output| {
            writeln!(
                output,
                "{{\"schema_version\":1,\"type\":\"doctor\",\"ready\":{all_ready},\"media_tools_ready\":{ready},\"proxy_ready\":{proxy_ready},\"audio_ready\":{audio_ready},\"audio_encoders\":{{\"mp3\":{},\"wav\":{},\"flac\":{}}},\"ffmpeg\":{},\"ffprobe\":{},\"ffmpeg_encoder_check_error\":{}}}",
                has_encoder("libmp3lame"),
                has_encoder("pcm_s16le"),
                has_encoder("flac"),
                tool_json(&ffmpeg),
                tool_json(&ffprobe),
                encoders
                    .as_ref()
                    .err()
                    .map(|error| json_string(error))
                    .unwrap_or_else(|| "null".to_owned())
            )
        })?;
    } else {
        println!("MediaForge {VERSION} · {}", env::consts::OS);
        for (name, result) in [("ffmpeg", ffmpeg), ("ffprobe", ffprobe)] {
            match result {
                Ok(version) => println!("{name}: {version}"),
                Err(error) => println!("{name}: {error}"),
            }
        }
        println!(
            "Media commands: {}",
            if ready {
                "ready"
            } else {
                "missing a required executable"
            }
        );
        println!(
            "Proxy encoders (H.264/AAC): {}",
            if proxy_ready { "available" } else { "missing" }
        );
        println!(
            "Audio encoders (MP3/WAV/FLAC): {}/{}/{}",
            if has_encoder("libmp3lame") {
                "available"
            } else {
                "missing"
            },
            if has_encoder("pcm_s16le") {
                "available"
            } else {
                "missing"
            },
            if has_encoder("flac") {
                "available"
            } else {
                "missing"
            }
        );
        println!(
            "All media features: {}",
            if all_ready {
                "ready"
            } else {
                "missing a required executable or encoder"
            }
        );
    }
    if strict && !all_ready {
        return Err("doctor found missing required media tools or encoders".to_owned());
    }
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
        Some("scan-config") => run_scan_config(&args[1..]),
        Some("snapshot") => run_snapshot(&args[1..]),
        Some("dupes") => run_dupes(&args[1..]),
        Some("probe") => run_probe(&args[1..]),
        Some("probe-dir") => run_probe_dir(&args[1..]),
        Some("storyboard") => run_storyboard(&args[1..]),
        Some("scene-detect") => run_scene_detect(&args[1..]),
        Some("scene-detect-dir") => run_scene_detect_dir(&args[1..]),
        Some("proxy") => run_proxy(&args[1..]),
        Some("proxy-dir") => run_proxy_dir(&args[1..]),
        Some("proxy-config") => run_proxy_config(&args[1..]),
        Some("extract-audio") => run_extract_audio(&args[1..]),
        Some("extract-audio-dir") => run_extract_audio_dir(&args[1..]),
        Some("project") => run_project(&args[1..]),
        Some("unity-meta") => run_unity_meta(&args[1..]),
        Some("gitignore") => run_gitignore(&args[1..]),
        Some("organize") => run_organize(&args[1..]),
        Some("rename") => run_rename(&args[1..]),
        Some("sequence") => run_sequence(&args[1..]),
        Some("undo") => run_undo(&args[1..]),
        Some("pack") => run_pack(&args[1..]),
        Some("doctor") => run_doctor(&args[1..]),
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
