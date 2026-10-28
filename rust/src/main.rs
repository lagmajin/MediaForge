use std::borrow::Cow;
use std::collections::{BTreeMap, BinaryHeap, HashMap, HashSet};
use std::env;
use std::fmt::Write as FmtWrite;
use std::fs::{self, File, OpenOptions};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{self, BufRead, BufReader, BufWriter, Cursor, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
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
    let Some(extension) = path.extension().and_then(|extension| extension.to_str()) else {
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
    path.file_name()
        .is_some_and(|name| name.to_string_lossy().starts_with('.'))
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

fn is_hidden(path: &Path) -> bool {
    if is_dot_hidden(path) {
        return true;
    }
    #[cfg(windows)]
    {
        fs::symlink_metadata(path).is_ok_and(|metadata| metadata_has_hidden_attribute(&metadata))
    }
    #[cfg(not(windows))]
    {
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

fn sort_entries(entries: &mut Vec<fs::DirEntry>) {
    let mut keyed = entries
        .drain(..)
        .map(|entry| {
            let original_name = entry.file_name().to_string_lossy().into_owned();
            let lowercase_name = original_name.to_lowercase();
            (lowercase_name, original_name, entry)
        })
        .collect::<Vec<_>>();
    keyed.sort_unstable_by(|(left_lower, left_name, _), (right_lower, right_name, _)| {
        left_lower
            .cmp(right_lower)
            .then_with(|| left_name.cmp(right_name))
    });
    entries.extend(keyed.into_iter().map(|(_, _, entry)| entry));
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

fn walk_with_skip_dirs_mode_counted(
    root: &Path,
    current: &Path,
    include_hidden: bool,
    skip_dirs: &[&str],
    ignored: &[IgnoreRule],
    filters: &ScanTraversalFilters<'_>,
    records: &mut Vec<FileRecord>,
    strict: bool,
    skipped_paths: &mut usize,
    skipped_examples: &mut Vec<String>,
) -> Result<(), String> {
    let mut pending = vec![(
        current.to_path_buf(),
        Arc::<[IgnoreRule]>::from(ignored),
        filters.path_contains_lower.is_empty(),
    )];
    let mut lowercase_scratch = String::new();
    while let Some((directory, active_ignored, path_filter_satisfied)) = pending.pop() {
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(_) if !strict => {
                record_skipped_path(&directory, skipped_paths, skipped_examples);
                continue;
            }
            Err(error) => {
                return Err(format!(
                    "cannot read directory {}: {error}",
                    directory.display()
                ));
            }
        };
        let mut entries = entries
            .map(|entry| match entry {
                Ok(entry) => Ok(entry),
                Err(_) if !strict => {
                    record_skipped_path(&directory, skipped_paths, skipped_examples);
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
            .map_err(|error| format!("cannot read entry in {}: {error}", directory.display()))?;
        sort_entries(&mut entries);
        for entry in entries {
            let path = entry.path();
            if !include_hidden && is_dot_hidden(&path) {
                continue;
            }
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(_) if !strict => {
                    record_skipped_path(&path, skipped_paths, skipped_examples);
                    continue;
                }
                Err(error) => return Err(format!("cannot inspect {}: {error}", path.display())),
            };
            if file_type.is_symlink() {
                continue;
            }
            let needs_relative_lower = !filters.exclude_path_contains_lower.is_empty()
                || (!path_filter_satisfied && !filters.path_contains_lower.is_empty());
            let mut relative = (file_type.is_dir() || needs_relative_lower).then(|| {
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
                let relative = relative
                    .as_deref()
                    .expect("directory paths are materialized");
                let name = entry.file_name();
                let name = name.to_string_lossy();
                let child_path_filter_satisfied = path_filter_satisfied
                    || filters.path_contains_lower.iter().all(|needle| {
                        relative_lower
                            .as_ref()
                            .is_some_and(|value| value.contains(needle))
                    });
                if directory_name_matches(&name, skip_dirs)
                    || (!include_hidden && is_hidden(&path))
                    || is_ignored_directory(relative, &active_ignored)
                    || filters.exclude_path_contains_lower.iter().any(|needle| {
                        relative_lower
                            .as_ref()
                            .is_some_and(|value| value.contains(needle))
                    })
                {
                    continue;
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
                    continue;
                }
                let kind = classify(&path);
                if filters.kind.is_some_and(|filter| filter != kind) {
                    continue;
                }
                let extension_value = path.extension().and_then(|value| value.to_str());
                if filters.extensions.is_some_and(|extensions| {
                    !matches_extension_filter(extension_value, extensions)
                }) {
                    continue;
                }
                if !active_ignored.is_empty() {
                    let relative = relative.get_or_insert_with(|| {
                        path.strip_prefix(root)
                            .unwrap_or(&path)
                            .to_string_lossy()
                            .replace('\\', "/")
                    });
                    if is_ignored(relative, &active_ignored) {
                        continue;
                    }
                }
                let metadata = match entry.metadata() {
                    Ok(metadata) => metadata,
                    Err(_) if !strict => {
                        record_skipped_path(&path, skipped_paths, skipped_examples);
                        continue;
                    }
                    Err(error) => {
                        return Err(format!("cannot inspect {}: {error}", path.display()));
                    }
                };
                if !include_hidden && metadata_has_hidden_attribute(&metadata) {
                    continue;
                }
                let size = metadata.len();
                if filters.min_size.is_some_and(|minimum| size < minimum)
                    || filters.max_size.is_some_and(|maximum| size > maximum)
                {
                    continue;
                }
                let relative = relative.take().unwrap_or_else(|| {
                    path.strip_prefix(root)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .replace('\\', "/")
                });
                let extension = extension_value
                    .map(|value| format!(".{}", value.to_ascii_lowercase()))
                    .unwrap_or_else(|| "[no extension]".to_owned());
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
                        continue;
                    };
                    if filters
                        .modified_since
                        .is_some_and(|since| modified_seconds < since)
                        || filters
                            .modified_until
                            .is_some_and(|until| modified_seconds > until)
                    {
                        continue;
                    }
                }
                records.push(FileRecord {
                    relative,
                    kind,
                    modified: if filters.include_modified {
                        modified
                    } else {
                        None
                    },
                    size,
                    extension,
                    path: PathBuf::new(),
                });
            }
        }
    }
    Ok(())
}

fn record_skipped_path(path: &Path, count: &mut usize, examples: &mut Vec<String>) {
    const MAX_EXAMPLES: usize = 100;
    *count = count.saturating_add(1);
    if examples.len() < MAX_EXAMPLES {
        examples.push(path.to_string_lossy().into_owned());
    }
}

fn walk_strict(
    root: &Path,
    current: &Path,
    include_hidden: bool,
    skip_dirs: &[&str],
    ignored: &[IgnoreRule],
    records: &mut Vec<FileRecord>,
) -> Result<(), String> {
    walk_strict_filtered(
        root,
        current,
        include_hidden,
        skip_dirs,
        ignored,
        &StrictWalkFilters::default(),
        records,
    )
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
        let mut entries = entries
            .map(|entry| {
                entry.map_err(|error| {
                    format!("cannot read entry in {}: {error}", directory.display())
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        if filters.sort_entries_per_directory {
            sort_entries(&mut entries);
        }
        for entry in entries {
            let path = entry.path();
            if !include_hidden && is_dot_hidden(&path) {
                continue;
            }
            let file_type = entry
                .file_type()
                .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
            if file_type.is_symlink() {
                continue;
            }
            let mut relative = if file_type.is_dir() {
                Some(
                    path.strip_prefix(root)
                        .map_err(|error| format!("cannot relativize {}: {error}", path.display()))?
                        .to_string_lossy()
                        .replace('\\', "/"),
                )
            } else {
                None
            };
            if file_type.is_dir() {
                let relative = relative
                    .as_deref()
                    .expect("directory paths are materialized");
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if directory_name_matches(&name, skip_dirs)
                    || (!include_hidden && is_hidden(&path))
                    || is_ignored_directory(relative, &active_ignored)
                    || filters
                        .excluded_subtree
                        .is_some_and(|excluded| path.starts_with(excluded))
                    || filters.exclude_path_contains_lower.is_some_and(|values| {
                        if values.is_empty() {
                            return false;
                        }
                        lowercase_into(relative, &mut lowercase_scratch);
                        values.iter().any(|value| lowercase_scratch.contains(value))
                    })
                {
                    continue;
                }
                let child_rules = extend_ignore_rules(root, &path, active_ignored.as_ref())?;
                let child_ignored = match child_rules {
                    Cow::Borrowed(_) => Arc::clone(&active_ignored),
                    Cow::Owned(rules) => Arc::from(rules),
                };
                pending.push((path, child_ignored));
            } else if file_type.is_file() {
                let kind = classify(&path);
                if filters.kinds.is_some_and(|values| !values.contains(&kind)) {
                    continue;
                }
                let extension_value = path.extension().and_then(|value| value.to_str());
                if filters
                    .extensions
                    .is_some_and(|values| !matches_extension_filter(extension_value, values))
                {
                    continue;
                }
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
                        continue;
                    }
                }
                let metadata = entry
                    .metadata()
                    .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
                if !include_hidden && metadata_has_hidden_attribute(&metadata) {
                    continue;
                }
                let size = metadata.len();
                if filters.min_size.is_some_and(|minimum| size < minimum)
                    || filters.max_size.is_some_and(|maximum| size > maximum)
                {
                    continue;
                }
                let relative = match relative.take() {
                    Some(relative) => relative,
                    None => path
                        .strip_prefix(root)
                        .map_err(|error| format!("cannot relativize {}: {error}", path.display()))?
                        .to_string_lossy()
                        .replace('\\', "/"),
                };
                let extension = if filters.omit_extension && filters.extensions.is_none() {
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
        }
    }
    Ok(())
}

#[derive(Clone, Debug)]
struct IgnoreRule {
    pattern: Vec<GlobToken>,
    base: String,
    negated: bool,
    anchored: bool,
    directory_only: bool,
    has_slash: bool,
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
    Ok(extend_ignore_rules(root, root, &[])?.into_owned())
}

fn extend_ignore_rules<'a>(
    root: &Path,
    directory: &Path,
    inherited: &'a [IgnoreRule],
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
    let base = directory
        .strip_prefix(root)
        .map_err(|error| format!("cannot relativize {}: {error}", directory.display()))?
        .to_string_lossy()
        .replace('\\', "/");
    let mut local_rules = Vec::new();
    for mut rule in text.lines().filter_map(parse_ignore_rule) {
        rule.base = base.clone();
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
    let has_slash = pattern
        .iter()
        .any(|token| matches!(token, GlobToken::Literal('/') | GlobToken::AnyDirectories));
    Some(IgnoreRule {
        pattern,
        base: String::new(),
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

fn parse_glob_pattern(pattern: &str, anchored: bool) -> Option<Vec<GlobToken>> {
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
    Some(tokens)
}

fn glob_matches(pattern: &[GlobToken], value: &str, scratch: &mut GlobMatchScratch) -> bool {
    if pattern
        .iter()
        .all(|token| matches!(token, GlobToken::Literal(_)))
    {
        let mut characters = value.chars();
        return pattern.iter().all(|token| {
            matches!(token, GlobToken::Literal(expected) if Some(*expected) == characters.next())
        }) && characters.next().is_none();
    }
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
    pattern: &[GlobToken],
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
        .strip_prefix(&rule.base)
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
    GLOB_MATCH_SCRATCH.with(|scratch| {
        let mut scratch = scratch.borrow_mut();
        rules
            .iter()
            .rev()
            .find(|rule| rule_matches_path(rule, relative, is_directory, &mut scratch))
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

fn write_json_string(output: &mut impl Write, value: &str) -> io::Result<()> {
    output.write_all(b"\"")?;
    for character in value.chars() {
        match character {
            '"' => output.write_all(b"\\\"")?,
            '\\' => output.write_all(b"\\\\")?,
            '\n' => output.write_all(b"\\n")?,
            '\r' => output.write_all(b"\\r")?,
            '\t' => output.write_all(b"\\t")?,
            character if character <= '\u{1f}' => {
                write!(output, "\\u{:04x}", character as u32)?;
            }
            character => {
                let mut encoded = [0_u8; 4];
                output.write_all(character.encode_utf8(&mut encoded).as_bytes())?;
            }
        }
    }
    output.write_all(b"\"")
}

fn scan_filters_json(
    kind_filter: Option<Kind>,
    min_size: Option<u64>,
    max_size: Option<u64>,
    modified_since: Option<u64>,
    modified_until: Option<u64>,
    modified_within: Option<u64>,
    path_contains: &[String],
    exclude_path_contains: &[String],
    extension_filter: Option<&HashSet<String>>,
    include_hidden: bool,
    strict: bool,
) -> String {
    let extensions = extension_filter.map_or_else(
        || "null".to_owned(),
        |extensions| {
            let mut extensions = extensions.iter().collect::<Vec<_>>();
            extensions.sort();
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
        kind_filter.map_or("null".to_owned(), |kind| json_string(kind.as_str())),
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

fn print_scan_filter_preview(
    path: &Path,
    json: bool,
    csv: bool,
    csv_sort: Option<ScanCsvSort>,
    largest_limit: usize,
    largest_limit_default: usize,
    largest_limit_is_explicit: bool,
    kind_filter: Option<Kind>,
    min_size: Option<u64>,
    max_size: Option<u64>,
    modified_since: Option<u64>,
    modified_until: Option<u64>,
    modified_within: Option<u64>,
    path_contains: &[String],
    exclude_path_contains: &[String],
    extension_filter: Option<&HashSet<String>>,
    include_hidden: bool,
    strict: bool,
) {
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
    println!(
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
        scan_filters_json(
            kind_filter,
            min_size,
            max_size,
            modified_since,
            modified_until,
            modified_within,
            path_contains,
            exclude_path_contains,
            extension_filter,
            include_hidden,
            strict,
        ),
    );
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

fn compare_largest_records(left: &FileRecord, right: &FileRecord) -> std::cmp::Ordering {
    std::cmp::Reverse(left.size)
        .cmp(&std::cmp::Reverse(right.size))
        .then_with(|| compare_paths_case_insensitive(&left.relative, &right.relative))
}

struct LargestRecord<'a>(&'a FileRecord);

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

fn largest_records(records: &[FileRecord], limit: usize) -> Vec<&FileRecord> {
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
    records: &[FileRecord],
    largest_limit: usize,
    include_same_size: bool,
    skipped_paths: usize,
    skipped_examples: &[String],
    kind_filter: Option<Kind>,
    min_size: Option<u64>,
    max_size: Option<u64>,
    modified_since: Option<u64>,
    modified_until: Option<u64>,
    modified_within: Option<u64>,
    path_contains: &[String],
    exclude_path_contains: &[String],
    extension_filter: Option<&HashSet<String>>,
    include_hidden: bool,
    strict: bool,
) -> Result<(), String> {
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
    if include_same_size {
        for (size, count) in by_size {
            if count > 1 {
                groups.push((size, count, Vec::<&str>::with_capacity(count)));
            }
        }
        groups.sort_by_key(|(size, _, _)| *size);
    }
    if include_same_size {
        for record in records {
            if let Ok(index) = groups.binary_search_by_key(&record.size, |(size, _, _)| *size) {
                groups[index].2.push(&record.relative);
            }
        }
        for (_, _, files) in &mut groups {
            files.sort_unstable_by(|left, right| compare_paths_case_insensitive(left, right));
        }
    }
    let stdout = io::stdout();
    let mut output = io::BufWriter::with_capacity(64 * 1024, stdout.lock());
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
    write_scan_json_line!(
        "  \"filters\": {},",
        scan_filters_json(
            kind_filter,
            min_size,
            max_size,
            modified_since,
            modified_until,
            modified_within,
            path_contains,
            exclude_path_contains,
            extension_filter,
            include_hidden,
            strict
        )
    );
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
    ext_sorted.sort_by(|a, b| b.1.0.cmp(&a.1.0).then_with(|| a.0.cmp(b.0)));
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

fn print_report(
    root: &Path,
    records: &[FileRecord],
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

fn write_csv_field(output: &mut impl Write, value: &str) -> io::Result<()> {
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

fn print_csv(records: &[FileRecord]) -> Result<(), String> {
    let stdout = io::stdout();
    let mut output = io::BufWriter::new(stdout.lock());
    let result = (|| -> io::Result<()> {
        output.write_all(b"path,type,extension,size_bytes,modified_unix_seconds\n")?;
        for record in records {
            write_csv_field(&mut output, &record.relative)?;
            output.write_all(b",")?;
            write_csv_field(&mut output, record.kind.as_str())?;
            output.write_all(b",")?;
            write_csv_field(&mut output, &record.extension)?;
            write!(output, ",{}", record.size)?;
            let modified = record
                .modified
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|duration| duration.as_secs());
            if let Some(modified) = modified {
                writeln!(output, ",{}", modified)?;
            } else {
                output.write_all(b",\n")?;
            }
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
    kind_filter: Option<Kind>,
    min_size: Option<u64>,
    max_size: Option<u64>,
    modified_since: Option<u64>,
    modified_until: Option<u64>,
    modified_within: Option<u64>,
    path_contains: &[String],
    exclude_path_contains: &[String],
    extension_filter: Option<&HashSet<String>>,
    include_hidden: bool,
    strict: bool,
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
        scan_filters_json(
            kind_filter,
            min_size,
            max_size,
            modified_since,
            modified_until,
            modified_within,
            path_contains,
            exclude_path_contains,
            extension_filter,
            include_hidden,
            strict,
        ),
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
            let mut settings = Vec::new();
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
                settings.push(format!(
                    "{{\"key\":{},\"value\":{}}}",
                    json_string(key.trim()),
                    json_string(value.trim())
                ));
            }
            println!(
                "{{\"schema_version\":1,\"type\":\"scan_settings\",\"path\":{},\"settings\":[{}]}}",
                json_string(&path.to_string_lossy()),
                settings.join(",")
            );
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
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|error| {
            if error.kind() == io::ErrorKind::AlreadyExists {
                format!(
                    "settings file already exists; refusing to overwrite: {}",
                    path.display()
                )
            } else {
                format!("cannot create scan settings {}: {error}", path.display())
            }
        })?;
    file.write_all(contents.as_bytes())
        .map_err(|error| format!("could not write scan settings {}: {error}", path.display()))?;
    println!("Created scan settings {}", path.display());
    Ok(())
}

fn files_identical(left: &Path, right: &Path, size: u64) -> io::Result<bool> {
    const DEFAULT_BUFFER_SIZE: usize = 8 * 1024;
    const MAX_BUFFER_SIZE: usize = 256 * 1024;
    let capacity = usize::try_from(size)
        .unwrap_or(MAX_BUFFER_SIZE)
        .clamp(DEFAULT_BUFFER_SIZE, MAX_BUFFER_SIZE);
    let mut left = BufReader::with_capacity(capacity, fs::File::open(left)?);
    let mut right = BufReader::with_capacity(capacity, fs::File::open(right)?);
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
    if jobs == 1 || paths.len() < 8 {
        for (index, relative) in paths.iter().enumerate() {
            visit(
                index,
                sample_signature_for_path(root, relative.as_ref(), size),
            )?;
        }
        return Ok(());
    }
    let worker_count = jobs.min(paths.len());
    let chunk_size = paths.len().div_ceil(worker_count);
    let results = std::thread::scope(|scope| {
        let handles = paths
            .chunks(chunk_size)
            .map(|chunk| {
                scope.spawn(move || {
                    chunk
                        .iter()
                        .map(|relative| sample_signature_for_path(root, relative.as_ref(), size))
                        .collect::<Vec<_>>()
                })
            })
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("duplicate sampling worker panicked"))
            .collect::<Vec<_>>()
    });
    let mut index = 0;
    for chunk_results in results {
        for result in chunk_results {
            visit(index, result)?;
            index += 1;
        }
    }
    Ok(())
}

struct VerifiedDuplicateGroup {
    representative: usize,
    files: Option<Vec<usize>>,
    count: usize,
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

fn compare_duplicate_output_groups(
    left: &DuplicateOutputGroup,
    right: &DuplicateOutputGroup,
) -> std::cmp::Ordering {
    let left_reclaimable = u128::from(left.size) * left.files.len().saturating_sub(1) as u128;
    let right_reclaimable = u128::from(right.size) * right.files.len().saturating_sub(1) as u128;
    right_reclaimable
        .cmp(&left_reclaimable)
        .then_with(|| right.size.cmp(&left.size))
        .then_with(|| left.files.first().cmp(&right.files.first()))
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
                    "Usage: mediaforge dupes [PATH] [--json] [--summary] [--include-hidden] [--exclude-path-contains TEXT ...] [--type TYPE] [--min-size SIZE] [--max-size SIZE] [--extension EXTENSIONS] [--top-groups N] [--jobs N]\n\nFind byte-for-byte duplicate files. Files are compared in fixed-size chunks to keep memory use low. --summary verifies every candidate but omits duplicate paths from output. Repeat --exclude-path-contains to skip matching paths case-insensitively; matching directories are pruned before traversal. --jobs controls concurrent sample reads (1–64; default up to 4). --min-size and --max-size limit which file sizes are checked; both accept bytes or K/M/G/T units. --top-groups limits displayed groups, ordered by reclaimable space; summary totals include all groups. --type accepts video, audio, image, model, texture, project, or other. --extension accepts a comma-separated list such as mov,mxf; use none to select files without an extension. Fractional size thresholds round up to the next byte."
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
    for (size, paths) in duplicate_sizes {
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
                match files_identical(&representative, &candidate, size) {
                    Ok(true) => {
                        group.count += 1;
                        if retain_group_paths {
                            let files = group
                                .files
                                .get_or_insert_with(|| vec![group.representative]);
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
                let files = file_indices
                    .into_iter()
                    .map(|index| paths[index].to_string())
                    .collect();
                let candidate = DuplicateOutputGroup { size, files };
                if let Some(limit) = top_groups {
                    if top_group_heap.len() < limit {
                        top_group_heap.push(candidate);
                    } else if top_group_heap.peek().is_some_and(|worst| {
                        compare_duplicate_output_groups(&candidate, worst).is_lt()
                    }) {
                        top_group_heap.pop();
                        top_group_heap.push(candidate);
                    }
                } else {
                    groups.push((candidate.size, candidate.files));
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
        groups.sort_by(|left, right| {
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
        println!(
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
        );
        let stdout = io::stdout();
        let mut output = io::BufWriter::new(stdout.lock());
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
        output
            .flush()
            .map_err(|error| format!("could not write duplicate JSON: {error}"))?;
        println!(
            "  ],\n  \"duplicate_files\": {},\n  \"reclaimable_bytes\": {},\n  \"reclaimable_size\": {}\n}}",
            duplicate_files,
            reclaimable,
            json_string(&human_size_wide(reclaimable))
        );
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
                    "Usage: mediaforge scan [PATH] [--json|--csv] [--csv-info] [--show-filters] [--include-hidden] [--strict] [--no-same-size] [--type TYPE] [--min-size SIZE] [--max-size SIZE] [--modified-since UNIX_SECONDS] [--modified-until UNIX_SECONDS] [--modified-within DURATION] [--path-contains TEXT ...] [--exclude-path-contains TEXT ...] [--extension EXTENSIONS ...] [--largest N] [--sort path|size|modified] [--settings FILE]\n\nSummarize media and game assets in a folder. --show-filters prints the resolved filters as JSON and exits without scanning the target path, so it can preview --settings plus command-line overrides. The preview reports whether --largest applies and whether its value is explicit or the output-format default; CSV always includes all matching rows. --no-same-size omits the potentially large same-size candidate path list from JSON while retaining the group count. --min-size and --max-size select an inclusive byte range; fractional size values are rounded up to whole bytes. --modified-since and --modified-until select an inclusive modification-time range in Unix seconds. --modified-within selects recent files using a duration such as 12h or 7d and cannot be combined with those absolute-time filters. --settings loads key=value scan filters; explicit command-line values are applied afterward. --path-contains selects relative paths by a case-insensitive substring; repeat it to require every substring. --exclude-path-contains removes paths matching any repeated case-insensitive substring. --extension accepts comma-separated extensions; repeat it to combine more extensions. Permission and file-system errors are skipped by default; text/JSON report incomplete scans and CSV writes warnings to stderr. JSON uses schema_version 1 and includes incomplete, skipped_paths, up to 100 skipped_path_examples, skipped_paths_truncated, and the applied filters. Use --strict to fail on the first traversal error instead. --csv emits one row per matched file with path, type, extension, byte size, and modification time in Unix seconds; it always includes all matches. Add --csv-info to write a JSON scan-info record to stderr without changing CSV stdout. Use --sort path, --sort size, or --sort modified to control CSV row order; size order is largest first and modified order is newest first, with unavailable times last and deterministic path tie-breaking. --largest controls how many biggest files are shown (default: 10 in text, 20 in JSON). Defaults to the current directory.\n\nTypes: video, audio, image, model, texture, project, other\nExtensions: comma-separated, case-insensitive (for example exr,tif); use none to select files without an extension\nSizes: bytes, KiB/MiB/GiB, or K/M/G (for example 500M); fractional values round up to the next byte"
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
        print_scan_filter_preview(
            &path,
            json,
            csv,
            csv_sort,
            largest_limit,
            largest_limit_default,
            largest_limit_is_explicit,
            kind_filter,
            min_size,
            max_size,
            modified_since,
            modified_until,
            modified_within,
            &path_contains,
            &exclude_path_contains,
            extension_filter.as_ref(),
            include_hidden,
            strict,
        );
        return Ok(());
    }
    let root =
        fs::canonicalize(&path).map_err(|e| format!("cannot access {}: {e}", path.display()))?;
    if !root.is_dir() {
        return Err(format!("expected a directory: {}", root.display()));
    }
    let ignored = ignored_by_git(&root)?;
    let mut records = Vec::new();
    let mut skipped_paths = 0_usize;
    let mut skipped_examples = Vec::new();
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
    };
    walk_with_skip_dirs_mode_counted(
        &root,
        &root,
        include_hidden,
        SKIP_DIRS,
        &ignored,
        &traversal_filters,
        &mut records,
        strict,
        &mut skipped_paths,
        &mut skipped_examples,
    )?;
    if let Some(sort) = csv_sort {
        match sort {
            ScanCsvSort::Size => records.sort_unstable_by(|left, right| {
                std::cmp::Reverse(left.size)
                    .cmp(&std::cmp::Reverse(right.size))
                    .then_with(|| compare_paths_case_insensitive(&left.relative, &right.relative))
            }),
            ScanCsvSort::Path => records.sort_by(|a, b| a.relative.cmp(&b.relative)),
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
                records.len(),
                skipped_paths,
                &skipped_examples,
                kind_filter,
                min_size,
                max_size,
                modified_since,
                modified_until,
                modified_within,
                &path_contains,
                &exclude_path_contains,
                extension_filter.as_ref(),
                include_hidden,
                strict,
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
        print_csv(&records)?;
    } else if json {
        print_json(
            &root,
            &records,
            largest_limit,
            include_same_size,
            skipped_paths,
            &skipped_examples,
            kind_filter,
            min_size,
            max_size,
            modified_since,
            modified_until,
            modified_within,
            &path_contains,
            &exclude_path_contains,
            extension_filter.as_ref(),
            include_hidden,
            strict,
        )?;
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

fn snapshot_entry(record: &FileRecord, content_hash: [u8; 32]) -> SnapshotEntry {
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

fn hash_snapshot_file(record: &FileRecord) -> Result<[u8; 32], String> {
    let mut file = File::open(&record.path)
        .map_err(|error| format!("cannot read {} for hashing: {error}", record.path.display()))?;
    let before = file.metadata().map_err(|error| {
        format!(
            "cannot inspect {} before hashing: {error}",
            record.path.display()
        )
    })?;
    if before.len() != record.size || before.modified().ok() != record.modified {
        return Err(format!(
            "file changed during snapshot scan: {}",
            record.path.display()
        ));
    }
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 256 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| format!("could not hash {}: {error}", record.path.display()))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    let after = file.metadata().map_err(|error| {
        format!(
            "cannot inspect {} after hashing: {error}",
            record.path.display()
        )
    })?;
    if after.len() != record.size || after.modified().ok() != record.modified {
        return Err(format!(
            "file changed while hashing snapshot: {}",
            record.path.display()
        ));
    }
    Ok(hasher.finish())
}

fn collect_snapshot_entries(
    records: &[FileRecord],
    hash_content: bool,
    jobs: usize,
) -> Result<BTreeMap<&str, SnapshotEntry>, String> {
    if jobs == 0 {
        return Err("snapshot worker count must be greater than zero".to_owned());
    }
    let mut entries = BTreeMap::new();
    if hash_content && !records.is_empty() {
        let next = std::sync::atomic::AtomicUsize::new(0);
        let next = &next;
        let worker_count = jobs.min(records.len());
        let (sender, receiver) = std::sync::mpsc::sync_channel(worker_count.saturating_mul(2));
        let hash_error = std::thread::scope(|scope| {
            let mut handles = Vec::with_capacity(worker_count);
            for _ in 0..worker_count {
                let sender = sender.clone();
                handles.push(scope.spawn(move || {
                    loop {
                        let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(record) = records.get(index) else {
                            break;
                        };
                        if sender.send((index, hash_snapshot_file(record))).is_err() {
                            break;
                        }
                    }
                }));
            }
            drop(sender);
            let mut first_error = None;
            for (index, result) in receiver {
                match result {
                    Ok(content_hash) => {
                        let record = &records[index];
                        if entries
                            .insert(
                                record.relative.as_str(),
                                snapshot_entry(record, content_hash),
                            )
                            .is_some()
                            && first_error.is_none()
                        {
                            first_error = Some(format!(
                                "multiple files resolve to the same portable path: {}",
                                record.relative
                            ));
                        }
                    }
                    Err(error) if first_error.is_none() => first_error = Some(error),
                    Err(_) => {}
                }
            }
            for handle in handles {
                handle.join().expect("snapshot hashing worker panicked");
            }
            first_error
        });
        if let Some(error) = hash_error {
            return Err(error);
        }
        return Ok(entries);
    }
    for record in records {
        if entries
            .insert(record.relative.as_str(), snapshot_entry(record, [0; 32]))
            .is_some()
        {
            return Err(format!(
                "multiple files resolve to the same portable path: {}",
                record.relative
            ));
        }
    }
    Ok(entries)
}

fn snapshot_entries_equal(old: SnapshotEntry, new: SnapshotEntry, hash_content: bool) -> bool {
    if hash_content {
        old.content_hash == new.content_hash
    } else {
        old.size == new.size && old.modified_ns() == new.modified_ns()
    }
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

fn read_snapshot_manifest(
    path: &Path,
) -> Result<(bool, bool, bool, BTreeMap<String, SnapshotEntry>), String> {
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
    let mut entries = BTreeMap::new();
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
        match entries.entry(relative) {
            std::collections::btree_map::Entry::Occupied(entry) => {
                return Err(format!(
                    "snapshot contains a duplicate path: {}",
                    entry.key()
                ));
            }
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(snapshot_entry);
            }
        }
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
    let mut attempt = 0_u64;
    loop {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let candidate = parent.join(format!(
            ".mediaforge-snapshot-{}-{nonce}-{attempt}.tmp",
            std::process::id()
        ));
        attempt = attempt
            .checked_add(1)
            .ok_or_else(|| "could not allocate a temporary snapshot name".to_owned())?;
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => return Ok((candidate, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!(
                    "cannot create a temporary snapshot in {}: {error}",
                    parent.display()
                ));
            }
        }
    }
}

fn write_snapshot_manifest(
    output: &Path,
    records: &[FileRecord],
    entries: &BTreeMap<&str, SnapshotEntry>,
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
        for record in records {
            let path = record.relative.as_bytes();
            let length = u32::try_from(path.len())
                .map_err(|_| format!("snapshot path is too long: {}", record.relative))?;
            writer
                .write_all(&length.to_le_bytes())
                .and_then(|()| writer.write_all(path))
                .and_then(|()| writer.write_all(&record.size.to_le_bytes()))
                .map_err(|error| format!("could not write snapshot entry: {error}"))?;
            match entries
                .get(record.relative.as_str())
                .and_then(|entry| entry.modified_ns())
            {
                Some(value) => writer
                    .write_all(&[1])
                    .and_then(|()| writer.write_all(&value.to_le_bytes())),
                None => writer.write_all(&[0]),
            }
            .map_err(|error| format!("could not write snapshot timestamp: {error}"))?;
            if hash_content {
                writer
                    .write_all(&entries[record.relative.as_str()].content_hash)
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
                    "Usage: mediaforge snapshot [PATH] (--output FILE [--overwrite] | --diff FILE [--update]) [--json|--csv] [--summary] [--include-hidden] [--include-generated] [--hash] [--jobs N] [--fail-on-change]\n\nSave or compare project snapshots using relative paths, sizes, and modification times. --diff FILE --update replaces the baseline with the current project state after producing the diff. By default this is a fast metadata-only scan; --hash stores SHA-256 file fingerprints and compares contents, ignoring timestamp-only changes. --summary omits per-file diff rows while retaining totals, per-asset-type summaries, and five size-band summaries. --jobs controls concurrent file hashing (1–64; default up to 4). Unity/Unreal/Godot generated cache directories and hidden files are skipped by default. Snapshot files store inclusion settings and can be compared on another supported OS. --csv and --json select diff output formats. --fail-on-change applies only to --diff."
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

    let mut old_entries = BTreeMap::new();
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
    let mut records = Vec::new();
    walk_strict(&root, &root, use_hidden, skip_dirs, &ignored, &mut records)?;
    if let Some(excluded) = excluded {
        records.retain(|record| record.path != excluded);
    }
    records.sort_by(|left, right| left.relative.cmp(&right.relative));
    let current_entries = collect_snapshot_entries(&records, use_hash, jobs)?;
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
            println!(
                "{{\"type\":\"snapshot\",\"root\":{},\"output\":{},\"files\":{},\"bytes\":{total_bytes},\"include_hidden\":{use_hidden},\"include_generated\":{use_generated},\"hash\":{use_hash}}}",
                json_string(&root.to_string_lossy()),
                json_string(&output.to_string_lossy()),
                records.len()
            );
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
    for (relative, current) in &current_entries {
        let asset_type = classify(Path::new(relative)).as_str();
        let stats = by_type.entry(asset_type).or_default();
        match old_entries.get(*relative) {
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
    for (relative, previous) in &old_entries {
        if !current_entries.contains_key(relative.as_str()) {
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
        let mut stdout = stdout.lock();
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
            for (relative, current) in &current_entries {
                let old = old_entries.get(*relative).copied();
                let change = match old {
                    None => "added",
                    Some(previous) if !snapshot_entries_equal(previous, *current, use_hash) => {
                        "modified"
                    }
                    Some(_) => continue,
                };
                write_snapshot_csv_event(&mut stdout, change, relative, old, Some(*current))?;
            }
            for (relative, old) in &old_entries {
                if !current_entries.contains_key(relative.as_str()) {
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
    } else if json {
        let stdout = io::stdout();
        let mut output = io::BufWriter::with_capacity(64 * 1024, stdout.lock());
        if !summary_only {
            for (relative, current) in &current_entries {
                let old = old_entries.get(*relative);
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
            for (relative, old) in &old_entries {
                if current_entries.contains_key(relative.as_str()) {
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
            for (relative, current) in &current_entries {
                let asset_type = classify(Path::new(relative)).as_str();
                match old_entries.get(*relative) {
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
            for (relative, previous) in &old_entries {
                if !current_entries.contains_key(relative.as_str()) {
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
    let normalized = value.trim().to_ascii_lowercase();
    let split = normalized
        .find(|ch: char| !ch.is_ascii_digit() && ch != '.')
        .unwrap_or(normalized.len());
    let (number, unit) = normalized.split_at(split);
    let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
    if whole.is_empty() && fraction.is_empty()
        || !whole.bytes().all(|digit| digit.is_ascii_digit())
        || !fraction.bytes().all(|digit| digit.is_ascii_digit())
    {
        return Err(format!("invalid size: {value}"));
    }
    let multiplier = match unit.trim() {
        "" | "b" => 1_u128,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => 1024_u128.pow(2),
        "g" | "gb" | "gib" => 1024_u128.pow(3),
        "t" | "tb" | "tib" => 1024_u128.pow(4),
        other => return Err(format!("unknown size unit '{other}' in '{value}'")),
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
    let normalized = value.trim().to_ascii_lowercase();
    let split = normalized
        .find(|ch: char| !ch.is_ascii_digit())
        .unwrap_or(normalized.len());
    let (amount, unit) = normalized.split_at(split);
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
        "s" => 1,
        "m" => 60,
        "h" => 60 * 60,
        "d" => 24 * 60 * 60,
        "w" => 7 * 24 * 60 * 60,
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
    let mut product_digits = Vec::with_capacity(fraction.len() + 12);
    let mut carry = 0_u64;
    for digit in fraction.bytes().rev() {
        let product = u64::from(digit - b'0') * multiplier + carry;
        product_digits.push((product % 10) as u8);
        carry = product / 10;
    }
    while carry > 0 {
        product_digits.push((carry % 10) as u8);
        carry /= 10;
    }

    let fractional_digits = fraction.len();
    let mut scaled_integer = 0_u128;
    for digit in product_digits
        .get(fractional_digits..)
        .unwrap_or_default()
        .iter()
        .rev()
    {
        scaled_integer = scaled_integer * 10 + u128::from(*digit);
    }
    let has_remainder = product_digits[..fractional_digits.min(product_digits.len())]
        .iter()
        .any(|digit| *digit != 0);
    scaled_integer + u128::from(has_remainder)
}

fn print_help() {
    println!(
        "MediaForge {VERSION}\nFast, local-first CLI for game and video production assets.\n\nUsage:\n  mediaforge doctor [--json]\n  mediaforge scan-config <init|validate|show> [FILE] [--json]\n  mediaforge scan [PATH] [--json|--csv] [--csv-info] [--show-filters] [--include-hidden] [--strict] [--no-same-size] [--type TYPE] [--min-size SIZE] [--max-size SIZE] [--modified-since UNIX_SECONDS] [--modified-until UNIX_SECONDS] [--modified-within DURATION] [--path-contains TEXT ...] [--exclude-path-contains TEXT ...] [--extension EXTENSIONS ...] [--largest N] [--sort path|size|modified] [--settings FILE]\n  mediaforge dupes [PATH] [--json] [--summary] [--include-hidden] [--exclude-path-contains TEXT ...] [--type TYPE] [--min-size SIZE] [--max-size SIZE] [--extension EXTENSIONS] [--top-groups N] [--jobs N]\n  mediaforge probe <FILE> [--compact] [--summary]\n  mediaforge probe-dir <PATH> [--extension EXTENSIONS] [--include-hidden] [--type video|audio] [--min-size SIZE] [--max-size SIZE] [--jobs N] [--summary] [--compare] [--csv]\n  mediaforge storyboard <VIDEO> --output <IMAGE> [--interval SECONDS] [--columns N] [--rows N] [--width PIXELS]\n  mediaforge scene-detect-dir <PATH> [--threshold 0..1] [--min-gap SECONDS] [--segments] [--fps RATE] [--extension EXTENSIONS] [--include-hidden] [--jobs N]\n  mediaforge scene-detect <VIDEO> [--threshold 0..1] [--min-gap SECONDS] [--segments] [--format jsonl|csv] [--fps RATE]\n  mediaforge proxy <VIDEO> --output <FILE.mp4> [--profile fast|balanced|quality] [--settings FILE] [--max-width PIXELS] [--crf N] [--preset PRESET]\n  mediaforge proxy-dir <PATH> --output-dir <DIR> [--extension EXTENSIONS] [--include-hidden] [--profile fast|balanced|quality] [--settings FILE] [--max-width PIXELS] [--crf N] [--preset PRESET] [--jobs N] [--skip-existing] [--json]\n  mediaforge proxy-config init [FILE] [--profile fast|balanced|quality] [--max-width PIXELS] [--crf N] [--preset PRESET]\n  mediaforge extract-audio <VIDEO> --output <FILE> [--format mp3|wav|flac] [--bitrate RATE] [--stream N]\n  mediaforge extract-audio-dir <PATH> --output-dir <DIR> [--extension EXTENSIONS] [--include-hidden] [--format mp3|wav|flac] [--bitrate RATE] [--stream N] [--jobs N] [--skip-existing] [--json]\n  mediaforge snapshot [PATH] (--output FILE [--overwrite] | --diff FILE [--update]) [--json|--csv] [--summary] [--include-hidden] [--include-generated] [--hash] [--jobs N] [--fail-on-change]\n  mediaforge project [PATH] [--json]\n  mediaforge unity-meta [PROJECT_PATH] [--json] [--include-hidden]\n  mediaforge gitignore [PATH] [--apply] [--json]\n  mediaforge organize [PATH] [--by type|extension|size] [--type TYPE] [--extension EXTENSIONS] [--min-size SIZE] [--include-hidden] [--include-generated] [--json] [--apply]\n  mediaforge rename [PATH] [--replace OLD NEW] [--prefix TEXT] [--suffix TEXT] [--extension-case keep|lower|upper] [--extension EXTENSIONS] [--include-hidden] [--json] [--apply]
  mediaforge sequence [PATH] [--prefix NAME] [--start N] [--padding N] [--extension EXTENSIONS] [--include-hidden] [--natural] [--json] [--apply]\n  mediaforge undo [PATH] [--json]\n  mediaforge pack <PATH> --output <FILE.tar> [--type TYPES] [--extension EXTENSIONS] [--min-size SIZE] [--max-size SIZE] [--exclude-path-contains TEXT ...] [--include-hidden] [--include-generated] [--dry-run] [--json] [--list]\n  mediaforge --version\n  mediaforge --help"
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
    prefix: &str,
    fields: &mut BTreeMap<String, String>,
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
                let field = if prefix.is_empty() {
                    key.to_owned()
                } else {
                    format!("{prefix}.{key}")
                };
                collect_json_scalars(bytes, index, &field, fields, depth + 1)?;
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
                let field = format!("{prefix}[{element}]");
                collect_json_scalars(bytes, index, &field, fields, depth + 1)?;
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
            if !prefix.is_empty() {
                fields.insert(
                    prefix.to_owned(),
                    String::from_utf8_lossy(&bytes[start..*index]).into_owned(),
                );
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
            if !prefix.is_empty() {
                fields.insert(
                    prefix.to_owned(),
                    String::from_utf8_lossy(&bytes[start..*index]).into_owned(),
                );
            }
            Ok(())
        }
        None => Err("truncated ffprobe JSON output".to_owned()),
    }
}

fn summarize_probe_differences(
    records: &[FileRecord],
    results: &[(usize, bool, String, Option<String>)],
) -> Result<(), String> {
    const COMPARE_FIELDS: &[&str] = &[
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
    let mut values = BTreeMap::<String, BTreeMap<String, Vec<String>>>::new();
    let mut failures = Vec::new();
    let mut successful = 0_usize;
    for (index, success, json_line, probe) in results {
        if !success {
            failures.push((*index, json_line.as_str()));
            continue;
        }
        successful += 1;
        let probe = probe
            .as_ref()
            .ok_or_else(|| "missing successful ffprobe result".to_owned())?;
        let mut fields = BTreeMap::new();
        let mut cursor = 0;
        collect_json_scalars(probe.as_bytes(), &mut cursor, "", &mut fields, 0)?;
        for field in COMPARE_FIELDS {
            let value = fields
                .get(*field)
                .cloned()
                .unwrap_or_else(|| "null".to_owned());
            values
                .entry((*field).to_owned())
                .or_default()
                .entry(value)
                .or_default()
                .push(records[*index].relative.clone());
        }
    }
    let varying: Vec<_> = values
        .into_iter()
        .filter(|(_, values)| values.len() > 1)
        .collect();
    let mut output = String::new();
    let failed = failures.len();
    write!(
        output,
        "{{\"files\":{},\"successful\":{successful},\"failed\":{failed},\"varying_fields\":[",
        records.len()
    )
    .map_err(|error| format!("could not format comparison: {error}"))?;
    for (field_index, (field, groups)) in varying.iter().enumerate() {
        if field_index > 0 {
            output.push(',');
        }
        write!(output, "{{\"field\":{},\"values\":[", json_string(field))
            .map_err(|error| format!("could not format comparison: {error}"))?;
        for (value_index, (value, paths)) in groups.iter().enumerate() {
            if value_index > 0 {
                output.push(',');
            }
            write!(output, "{{\"value\":{value},\"files\":[")
                .map_err(|error| format!("could not format comparison: {error}"))?;
            for (path_index, path) in paths.iter().enumerate() {
                if path_index > 0 {
                    output.push(',');
                }
                output.push_str(&json_string(path));
            }
            output.push_str("]}");
        }
        output.push_str("]}");
    }
    output.push_str("],\"failures\":[");
    for (failure_index, (index, result)) in failures.iter().enumerate() {
        if failure_index > 0 {
            output.push(',');
        }
        let mut error_fields = BTreeMap::new();
        let mut cursor = 0;
        collect_json_scalars(result.as_bytes(), &mut cursor, "", &mut error_fields, 0)?;
        let error = error_fields
            .get("error")
            .map(String::as_str)
            .unwrap_or("null");
        write!(
            output,
            "{{\"path\":{},\"error\":{error}}}",
            json_string(&records[*index].relative)
        )
        .map_err(|error| format!("could not format comparison: {error}"))?;
    }
    output.push_str("]}");
    println!("{output}");
    Ok(())
}

fn probe_csv_value<'a>(fields: &'a BTreeMap<String, String>, name: &str) -> Cow<'a, str> {
    let Some(value) = fields.get(name) else {
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
    probe: Option<&str>,
) -> Result<(), String> {
    let mut fields = BTreeMap::new();
    if let Some(probe) = probe {
        let mut cursor = 0;
        collect_json_scalars(probe.as_bytes(), &mut cursor, "", &mut fields, 0)?;
    }
    let mut first = true;
    let result = (|| -> io::Result<()> {
        write_csv_column(output, &mut first, path)?;
        write_csv_column(output, &mut first, if ok { "true" } else { "false" })?;
        write_csv_column(output, &mut first, error)?;
        for name in [
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
        ] {
            write_csv_column(output, &mut first, &probe_csv_value(&fields, name))?;
        }
        output.write_all(b"\n")
    })();
    result.map_err(|write_error| format!("could not write probe CSV row: {write_error}"))
}

fn run_probe_dir(args: &[String]) -> Result<(), String> {
    const MAX_PROBE_JOBS: usize = 32;
    let mut include_hidden = false;
    let mut summary = false;
    let mut compare = false;
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
                    "Usage: mediaforge probe-dir <PATH> [--extension EXTENSIONS] [--include-hidden] [--type video|audio] [--min-size SIZE] [--max-size SIZE] [--jobs N] [--summary] [--compare] [--csv]\n\nProbe video and audio assets in a directory with ffprobe. Streams one JSON Lines result per file as probing finishes, with a completion counter. --csv writes one row per file with common video/audio metadata; failed probes are included with an error. --type filters to video or audio assets and selects only that stream type in the report. --min-size and --max-size filter file sizes before probing and accept bytes or K/M/G/T units. --jobs controls concurrent ffprobe processes (1–32). --summary selects compact fields. --compare implies --summary and emits only first-stream media-spec fields that differ across files, grouped by value. Use --type to compare video or audio streams consistently."
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
    let mut records = Vec::new();
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
        omit_modified_time: false,
        omit_extension: true,
    };
    walk_strict_filtered(
        &root,
        &root,
        include_hidden,
        SKIP_DIRS,
        &ignored,
        &filters,
        &mut records,
    )?;
    records.sort_by(|left, right| left.relative.cmp(&right.relative));
    if records.is_empty() {
        if csv {
            println!(
                "path,ok,error,format_name,duration_seconds,codec_type,codec,profile,width,height,frame_rate,pixel_format,sample_rate,channels,channel_layout,size_bytes,bit_rate"
            );
        } else if compare {
            println!(
                "{{\"files\":0,\"successful\":0,\"failed\":0,\"varying_fields\":[],\"failures\":[]}}"
            );
        }
        return Ok(());
    }

    let next = std::sync::atomic::AtomicUsize::new(0);
    let completed = std::sync::atomic::AtomicUsize::new(0);
    let stream_results = !csv && !compare;
    let stream_output = std::sync::Mutex::new(());
    let failures = std::sync::atomic::AtomicUsize::new(0);
    let results = std::sync::Mutex::new(Vec::<(usize, bool, String, Option<String>)>::new());
    let worker_count = jobs.min(records.len());
    std::thread::scope(|scope| {
        for _ in 0..worker_count {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(record) = records.get(index) else {
                        break;
                    };
                    let selector = selected_kind.map(|kind| match kind {
                        Kind::Video => "v",
                        Kind::Audio => "a",
                        _ => unreachable!(),
                    });
                    let result = ffprobe_json(&record.path, false, summary || csv, selector);
                    let (success, json, probe) = match result {
                        Ok(probe) => (
                            true,
                            format!(
                                "{{\"path\":{},\"ok\":true,\"probe\":{}}}",
                                json_string(&record.relative),
                                probe.trim()
                            ),
                            Some(probe),
                        ),
                        Err(error) => (
                            false,
                            format!(
                                "{{\"path\":{},\"ok\":false,\"error\":{}}}",
                                json_string(&record.relative),
                                json_string(&error)
                            ),
                            None,
                        ),
                    };
                    if !success {
                        failures.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    if stream_results {
                        let _guard = stream_output.lock().expect("probe output lock poisoned");
                        let current =
                            completed.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                        println!(
                            "{{\"type\":\"result\",\"current\":{current},\"total\":{},{}",
                            records.len(),
                            &json[1..]
                        );
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
    let mut results = results
        .into_inner()
        .map_err(|_| "probe results could not be collected".to_owned())?;
    if !stream_results {
        results.sort_by_key(|(index, _, _, _)| *index);
    }
    let failures = failures.load(std::sync::atomic::Ordering::Relaxed);
    if compare {
        summarize_probe_differences(&records, &results)?;
    } else if csv {
        let stdout = io::stdout();
        let mut output = io::BufWriter::with_capacity(64 * 1024, stdout.lock());
        output
            .write_all(b"path,ok,error,format_name,duration_seconds,codec_type,codec,profile,width,height,frame_rate,pixel_format,sample_rate,channels,channel_layout,size_bytes,bit_rate\n")
            .map_err(|error| format!("could not write probe CSV header: {error}"))?;
        for (index, success, record, probe) in &results {
            let error = if *success {
                String::new()
            } else {
                let mut fields = BTreeMap::new();
                let mut cursor = 0;
                collect_json_scalars(record.as_bytes(), &mut cursor, "", &mut fields, 0)?;
                probe_csv_value(&fields, "error").into_owned()
            };
            print_probe_csv_row(
                &mut output,
                &records[*index].relative,
                *success,
                &error,
                probe.as_deref(),
            )?;
        }
        output
            .flush()
            .map_err(|error| format!("could not write probe CSV: {error}"))?;
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
    let temporary = parent.join(format!(
        ".{stem}.mediaforge-{}-{}.tmp.{extension}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let tile_height = width * 9 / 16;
    let filter = format!(
        "fps=1/{interval},scale={width}:{tile_height}:force_original_aspect_ratio=decrease,pad={width}:{tile_height}:(ow-iw)/2:(oh-ih)/2,tile={columns}x{rows}"
    );
    let result = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-n", "-i"])
        .arg(&input)
        .args(["-vf", &filter, "-frames:v", "1"])
        .arg(&temporary)
        .output()
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                "ffmpeg was not found on PATH. Install FFmpeg to create storyboards.".to_owned()
            } else {
                format!("could not start ffmpeg: {error}")
            }
        })?;
    if !result.status.success() {
        let detail = String::from_utf8_lossy(&result.stderr).trim().to_owned();
        let _ = fs::remove_file(&temporary);
        return Err(if detail.is_empty() {
            format!("ffmpeg failed with {}", result.status)
        } else {
            format!("ffmpeg failed: {detail}")
        });
    }
    let temporary_size = match fs::metadata(&temporary) {
        Ok(metadata) => metadata.len(),
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            return Err(format!("could not inspect generated storyboard: {error}"));
        }
    };
    if temporary_size == 0 {
        let _ = fs::remove_file(&temporary);
        return Err("ffmpeg produced an empty storyboard".to_owned());
    }
    move_file_no_replace(&temporary, &output).map_err(|error| {
        let cleanup = fs::remove_file(&temporary);
        match cleanup {
            Ok(()) => format!("could not publish storyboard {}: {error}", output.display()),
            Err(cleanup_error) if cleanup_error.kind() == io::ErrorKind::NotFound => {
                format!("could not publish storyboard {}: {error}", output.display())
            }
            Err(cleanup_error) => format!(
                "could not publish storyboard {}: {error}; temporary image remains at {}: {cleanup_error}",
                output.display(),
                temporary.display()
            ),
        }
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

struct SceneSegment {
    index: usize,
    start: f64,
    end: f64,
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

fn run_scene_detect(args: &[String]) -> Result<(), String> {
    let mut input = None;
    let mut threshold = 0.4_f64;
    let mut min_gap = 1.0_f64;
    let mut segments = false;
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
            "--help" | "-h" => {
                println!(
                    "Usage: mediaforge scene-detect <VIDEO> [--threshold 0..1] [--min-gap SECONDS] [--segments] [--format jsonl|csv] [--fps RATE]\n\nFind likely cuts with FFmpeg's scene score. Defaults: threshold 0.4 and at least 1 second between results. JSON Lines is the default; CSV writes a header and one row per cut. --segments turns cut points into contiguous start/end intervals spanning the video. Frame numbers use the probed average rate; --fps overrides it (for example 30000/1001)."
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
    let input = input.ok_or_else(|| "scene-detect requires a video file".to_owned())?;
    let input = fs::canonicalize(&input)
        .map_err(|error| format!("cannot access video {}: {error}", input.display()))?;
    if !fs::metadata(&input)
        .map_err(|error| format!("cannot inspect video {}: {error}", input.display()))?
        .is_file()
    {
        return Err(format!("expected a video file: {}", input.display()));
    }
    let timing = probe_video_timing(&input)?;
    let fps = fps_override.unwrap_or(timing.fps);
    let nominal_fps = fps.round().max(1.0) as u32;
    let duration = if segments {
        Some(timing.duration.ok_or_else(|| {
            "ffprobe did not return a valid video duration required by --segments".to_owned()
        })?)
    } else {
        None
    };
    let filter = format!("select='gt(scene,{threshold})',metadata=mode=print");
    let result = Command::new("ffmpeg")
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
        .output()
        .map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                "ffmpeg was not found on PATH. Install FFmpeg to detect scene changes.".to_owned()
            } else {
                format!("could not start ffmpeg: {error}")
            }
        })?;
    if !result.status.success() {
        let detail = String::from_utf8_lossy(&result.stderr).trim().to_owned();
        return Err(if detail.is_empty() {
            format!("ffmpeg failed with {}", result.status)
        } else {
            format!("ffmpeg failed: {detail}")
        });
    }
    let diagnostics = String::from_utf8_lossy(&result.stderr);
    let mut current_time = None;
    let mut cuts = Vec::<SceneCut>::new();
    for line in diagnostics.lines() {
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
                if let Some(previous) = cuts.last_mut()
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
    let stdout = io::stdout();
    let mut output = io::BufWriter::with_capacity(64 * 1024, stdout.lock());
    let output_result = (|| -> io::Result<()> {
        if let Some(duration) = duration {
            let mut intervals = Vec::with_capacity(cuts.len() + 1);
            let mut start = 0.0_f64;
            for cut in &cuts {
                if cut.seconds > start && cut.seconds < duration {
                    intervals.push(SceneSegment {
                        index: intervals.len() + 1,
                        start,
                        end: cut.seconds,
                    });
                    start = cut.seconds;
                }
            }
            if start < duration {
                intervals.push(SceneSegment {
                    index: intervals.len() + 1,
                    start,
                    end: duration,
                });
            }
            if output_format == "csv" {
                output.write_all(b"path,segment,start_timecode,start_seconds,start_frame,end_timecode,end_seconds,end_frame,duration_seconds,fps\n")?;
                for interval in &intervals {
                    write_csv_field(&mut output, &input.to_string_lossy())?;
                    write!(output, ",{},", interval.index)?;
                    write_csv_field(&mut output, &scene_timecode(interval.start))?;
                    write!(
                        output,
                        ",{:.6},{},",
                        interval.start,
                        (interval.start * fps).round() as u64
                    )?;
                    write_csv_field(&mut output, &scene_timecode(interval.end))?;
                    writeln!(
                        output,
                        ",{:.6},{},{:.6},{:.6}",
                        interval.end,
                        (interval.end * fps).round() as u64,
                        interval.end - interval.start,
                        fps
                    )?;
                }
            } else {
                let source = json_string(&input.to_string_lossy());
                for interval in &intervals {
                    writeln!(
                        output,
                        "{{\"type\":\"segment\",\"path\":{source},\"segment\":{},\"start_timecode\":{},\"start_seconds\":{:.6},\"start_frame\":{},\"end_timecode\":{},\"end_seconds\":{:.6},\"end_frame\":{},\"duration_seconds\":{:.6},\"fps\":{fps:.6},\"nominal_fps\":{nominal_fps}}}",
                        interval.index,
                        json_string(&scene_timecode(interval.start)),
                        interval.start,
                        (interval.start * fps).round() as u64,
                        json_string(&scene_timecode(interval.end)),
                        interval.end,
                        (interval.end * fps).round() as u64,
                        interval.end - interval.start
                    )?;
                }
                writeln!(
                    output,
                    "{{\"type\":\"summary\",\"path\":{source},\"segments\":{},\"scene_cuts\":{},\"duration_seconds\":{duration:.6},\"fps\":{fps:.6},\"nominal_fps\":{nominal_fps},\"threshold\":{threshold:.6},\"min_gap_seconds\":{min_gap:.6}}}",
                    intervals.len(),
                    cuts.len()
                )?;
            }
        } else if output_format == "csv" {
            output.write_all(b"path,timecode,time_seconds,frame,fps,score\n")?;
            for cut in &cuts {
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
        } else {
            let source = json_string(&input.to_string_lossy());
            for cut in &cuts {
                writeln!(
                    output,
                    "{{\"type\":\"scene\",\"path\":{source},\"timecode\":{},\"time_seconds\":{:.6},\"frame\":{},\"fps\":{fps:.6},\"nominal_fps\":{nominal_fps},\"score\":{:.6}}}",
                    json_string(&scene_timecode(cut.seconds)),
                    cut.seconds,
                    (cut.seconds * fps).round() as u64,
                    cut.score
                )?;
            }
            writeln!(
                output,
                "{{\"type\":\"summary\",\"path\":{source},\"cuts\":{},\"fps\":{fps:.6},\"nominal_fps\":{nominal_fps},\"threshold\":{threshold:.6},\"min_gap_seconds\":{min_gap:.6}}}",
                cuts.len()
            )?;
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
            "--include-hidden" => include_hidden = true,
            "--help" | "-h" => {
                println!(
                    "Usage: mediaforge scene-detect-dir <PATH> [--threshold 0..1] [--min-gap SECONDS] [--segments] [--fps RATE] [--extension EXTENSIONS] [--include-hidden] [--jobs N]\n\nFind likely scene cuts in every video under a directory. Streams a progress record and each video's JSON Lines results as processing finishes, followed by a batch summary. --segments emits contiguous intervals instead of individual cuts. --jobs controls concurrent FFmpeg processes (1–16; default 4)."
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
    let mut records = Vec::new();
    let kinds = [Kind::Video];
    let filters = StrictWalkFilters {
        kinds: Some(&kinds),
        extensions: extensions.as_ref(),
        omit_extension: true,
        ..StrictWalkFilters::default()
    };
    walk_strict_filtered(
        &root,
        &root,
        include_hidden,
        SKIP_DIRS,
        &ignored,
        &filters,
        &mut records,
    )?;
    records.sort_by(|left, right| left.relative.cmp(&right.relative));

    let executable = env::current_exe()
        .map_err(|error| format!("could not locate the mediaforge executable: {error}"))?;
    let next = std::sync::atomic::AtomicUsize::new(0);
    let completed = std::sync::atomic::AtomicUsize::new(0);
    let stream_output = std::sync::Mutex::new(());
    let failed = std::sync::atomic::AtomicUsize::new(0);
    let first_output_error = std::sync::Mutex::new(None::<String>);
    std::thread::scope(|scope| {
        for _ in 0..jobs.min(records.len()) {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(record) = records.get(index) else {
                        break;
                    };
                    let mut command = Command::new(&executable);
                    command
                        .arg("scene-detect")
                        .arg(&record.path)
                        .args(["--threshold", &threshold.to_string()])
                        .args(["--min-gap", &min_gap.to_string()])
                        .args(["--format", "jsonl"]);
                    if segments {
                        command.arg("--segments");
                    }
                    if let Some(fps) = fps {
                        command.args(["--fps", &fps.to_string()]);
                    }
                    let result = command.output();
                    let (success, output) = match result {
                        Ok(output) if output.status.success() => {
                            (true, String::from_utf8_lossy(&output.stdout).into_owned())
                        }
                        Ok(output) => {
                            let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
                            let message = if message.is_empty() {
                                format!("scene-detect exited with {}", output.status)
                            } else {
                                message
                            };
                            (
                                false,
                                format!(
                                    "{{\"type\":\"error\",\"path\":{},\"error\":{}}}\n",
                                    json_string(&record.path.to_string_lossy()),
                                    json_string(&message)
                                ),
                            )
                        }
                        Err(error) => (
                            false,
                            format!(
                                "{{\"type\":\"error\",\"path\":{},\"error\":{}}}\n",
                                json_string(&record.path.to_string_lossy()),
                                json_string(&format!("could not start scene-detect: {error}"))
                            ),
                        ),
                    };
                    let write_error = {
                        let _guard = stream_output
                            .lock()
                            .expect("scene detection output lock poisoned");
                        let current = completed.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                            + 1;
                        let stdout = io::stdout();
                        let mut stdout = stdout.lock();
                        writeln!(
                            stdout,
                            "{{\"type\":\"progress\",\"current\":{current},\"total\":{},\"path\":{}}}",
                            records.len(),
                            json_string(&record.relative)
                        )
                        .and_then(|()| stdout.write_all(output.as_bytes()))
                        .err()
                        .map(|error| error.to_string())
                    };
                    if !success {
                        failed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    if let Some(error) = write_error {
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
    let temporary = parent.join(format!(
        ".mediaforge-proxy-{}-{}.tmp",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| format!("cannot create temporary settings file: {error}"))?;
    let write_result = file
        .write_all(contents.as_bytes())
        .and_then(|()| file.sync_all());
    drop(file);
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temporary);
        return Err(format!("could not write proxy settings: {error}"));
    }
    move_file_no_replace(&temporary, &path).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        format!(
            "could not publish proxy settings {}: {error}",
            path.display()
        )
    })?;
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
    let temporary = parent.join(format!(
        ".{stem}.mediaforge-{}-{}.tmp.mp4",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let filter = format!("scale=w='trunc(min({max_width},iw)/2)*2':h=-2");
    let result = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-n", "-i"])
        .arg(&input)
        .args(["-map", "0:v:0", "-map", "0:a?", "-sn", "-dn", "-vf"])
        .arg(filter)
        .args(["-c:v", "libx264", "-preset"])
        .arg(&preset)
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
        .arg(&temporary)
        .output()
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                "ffmpeg was not found on PATH. Install FFmpeg to create proxies.".to_owned()
            } else {
                format!("could not start ffmpeg: {error}")
            }
        })?;
    if !result.status.success() {
        let detail = String::from_utf8_lossy(&result.stderr).trim().to_owned();
        let _ = fs::remove_file(&temporary);
        return Err(if detail.is_empty() {
            format!("ffmpeg failed with {}", result.status)
        } else {
            format!("ffmpeg failed: {detail}")
        });
    }
    let temporary_size = match fs::metadata(&temporary) {
        Ok(metadata) => metadata.len(),
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            return Err(format!("could not inspect generated proxy: {error}"));
        }
    };
    if temporary_size == 0 {
        let _ = fs::remove_file(&temporary);
        return Err("ffmpeg produced an empty proxy".to_owned());
    }
    move_file_no_replace(&temporary, &output).map_err(|error| {
        let cleanup = fs::remove_file(&temporary);
        match cleanup {
            Ok(()) => format!("could not publish proxy {}: {error}", output.display()),
            Err(cleanup_error) if cleanup_error.kind() == io::ErrorKind::NotFound => {
                format!("could not publish proxy {}: {error}", output.display())
            }
            Err(cleanup_error) => format!(
                "could not publish proxy {}: {error}; temporary video remains at {}: {cleanup_error}",
                output.display(),
                temporary.display()
            ),
        }
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
) {
    let output = output
        .map(|path| json_string(&path.to_string_lossy()))
        .unwrap_or_else(|| "null".to_owned());
    match result {
        Ok(created) => println!(
            "{{\"type\":\"result\",\"current\":{current},\"total\":{total},\"path\":{},\"output\":{output},\"ok\":true,\"skipped\":{}}}",
            json_string(path),
            !created
        ),
        Err(error) => println!(
            "{{\"type\":\"result\",\"current\":{current},\"total\":{total},\"path\":{},\"output\":{output},\"ok\":false,\"error\":{}}}",
            json_string(path),
            json_string(error)
        ),
    }
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
            "--help" | "-h" => {
                println!(
                    "Usage: mediaforge proxy-dir <PATH> --output-dir <DIR> [--extension EXTENSIONS] [--include-hidden] [--profile fast|balanced|quality] [--settings FILE] [--max-width PIXELS] [--crf N] [--preset PRESET] [--jobs N] [--skip-existing] [--json]\n\nCreate H.264/AAC proxies for video assets while preserving their relative folders. Automatically reads .mediaforge-proxy from the working directory when present. Explicit options override settings-file values. --jobs controls concurrent ffmpeg processes (1–16; default 4). Existing regular output files are never overwritten; --skip-existing skips those files. Other occupied output paths are errors. Failed files are reported after the remaining videos are processed. The output directory must already exist. --json emits one result record per file as each job completes, with completion counters and a final summary as JSON Lines."
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
    let mut records = Vec::new();
    let kinds = [Kind::Video];
    let filters = StrictWalkFilters {
        kinds: Some(&kinds),
        extensions: extensions.as_ref(),
        excluded_subtree: output_inside_root.then_some(output_root.as_path()),
        omit_extension: true,
        ..StrictWalkFilters::default()
    };
    walk_strict_filtered(
        &root,
        &root,
        include_hidden,
        SKIP_DIRS,
        &ignored,
        &filters,
        &mut records,
    )?;
    records.sort_by(|left, right| left.relative.cmp(&right.relative));
    if records.is_empty() {
        if json {
            println!("{{\"type\":\"summary\",\"succeeded\":0,\"skipped\":0,\"failed\":0}}");
        } else {
            println!("No video assets found.");
        }
        return Ok(());
    }

    let total = records.len();
    let mut targets = HashSet::new();
    let plans: Vec<(Option<PathBuf>, Result<(), String>)> = records
        .iter()
        .map(|record| {
            let plan = (|| -> Result<PathBuf, String> {
                let relative = record
                    .path
                    .strip_prefix(&root)
                    .map_err(|error| format!("cannot relativize input path: {error}"))?;
                let stem = relative
                    .file_stem()
                    .and_then(|value| value.to_str())
                    .ok_or_else(|| "cannot create a proxy name from this path".to_owned())?;
                let mut relative_output = relative.to_path_buf();
                relative_output.set_file_name(format!("{stem}-proxy.mp4"));
                let output = output_root.join(relative_output);
                if !targets.insert(output.clone()) {
                    return Err(format!(
                        "multiple source files map to the same proxy path: {}",
                        output.display()
                    ));
                }
                Ok(output)
            })();
            match plan {
                Ok(output) => (Some(output), Ok(())),
                Err(error) => (None, Err(error)),
            }
        })
        .collect();
    let next = std::sync::atomic::AtomicUsize::new(0);
    let completed = std::sync::atomic::AtomicUsize::new(0);
    let progress_output = std::sync::Mutex::new(());
    let succeeded_count = std::sync::atomic::AtomicUsize::new(0);
    let skipped_count = std::sync::atomic::AtomicUsize::new(0);
    let failed_count = std::sync::atomic::AtomicUsize::new(0);
    let results = std::sync::Mutex::new(Vec::<(usize, Result<bool, String>)>::new());
    std::thread::scope(|scope| {
        for _ in 0..jobs.min(total) {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if index >= total {
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
                        let source_text = record
                            .path
                            .to_str()
                            .ok_or_else(|| "input path is not valid Unicode".to_owned())?;
                        let output_text = output
                            .to_str()
                            .ok_or_else(|| "output path is not valid Unicode".to_owned())?;
                        let proxy_args = vec![
                            source_text.to_owned(),
                            "--output".to_owned(),
                            output_text.to_owned(),
                            "--max-width".to_owned(),
                            max_width.to_string(),
                            "--crf".to_owned(),
                            crf.to_string(),
                            "--preset".to_owned(),
                            preset.clone(),
                            "--quiet".to_owned(),
                        ];
                        run_proxy(&proxy_args).map(|()| true)
                    });
                    if json {
                        let _guard = progress_output
                            .lock()
                            .expect("proxy progress output lock poisoned");
                        let current =
                            completed.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                        print_batch_result_json(
                            current,
                            total,
                            &record.relative,
                            output.as_deref(),
                            &result,
                        );
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
                        results
                            .lock()
                            .expect("proxy results lock poisoned")
                            .push((index, result));
                    }
                }
            });
        }
    });
    let mut results = results
        .into_inner()
        .map_err(|_| "proxy results lock poisoned".to_owned())?;
    let (succeeded, skipped, failed) = if json {
        (
            succeeded_count.load(std::sync::atomic::Ordering::Relaxed),
            skipped_count.load(std::sync::atomic::Ordering::Relaxed),
            failed_count.load(std::sync::atomic::Ordering::Relaxed),
        )
    } else {
        results.sort_by_key(|(index, _)| *index);
        let mut succeeded = 0_usize;
        let mut skipped = 0_usize;
        let mut failed = 0_usize;
        for (index, result) in results.drain(..) {
            let record = &records[index];
            match result {
                Ok(created) => {
                    if created {
                        succeeded += 1;
                    } else {
                        skipped += 1;
                    }
                    if let Some(output) = &plans[index].0 {
                        if created {
                            println!("Created proxy {}", output.display());
                        } else {
                            println!("Skipped existing proxy {}", output.display());
                        }
                    }
                }
                Err(error) => {
                    failed += 1;
                    eprintln!("mediaforge: {}: {error}", record.relative);
                }
            }
        }
        (succeeded, skipped, failed)
    };
    if json {
        println!(
            "{{\"type\":\"summary\",\"succeeded\":{succeeded},\"skipped\":{skipped},\"failed\":{failed}}}"
        );
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
        .is_none_or(|value| !value.eq_ignore_ascii_case(&format))
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
    let temporary = parent.join(format!(
        ".{stem}.mediaforge-{}-{}.tmp.{format}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let mut command = Command::new("ffmpeg");
    command
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-n", "-i"])
        .arg(&input)
        .args(["-map"])
        .arg(format!("0:a:{stream}"))
        .args(["-vn", "-sn", "-dn", "-c:a"]);
    match format.as_str() {
        "mp3" => {
            command.arg("libmp3lame").args(["-b:a", bitrate.as_str()]);
        }
        "wav" => {
            command.arg("pcm_s16le");
        }
        "flac" => {
            command.arg("flac");
        }
        _ => unreachable!(),
    }
    let result = command.arg(&temporary).output().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            "ffmpeg was not found on PATH. Install FFmpeg to extract audio.".to_owned()
        } else {
            format!("could not start ffmpeg: {error}")
        }
    })?;
    if !result.status.success() {
        let detail = String::from_utf8_lossy(&result.stderr).trim().to_owned();
        let _ = fs::remove_file(&temporary);
        return Err(if detail.is_empty() {
            format!("ffmpeg failed with {}", result.status)
        } else {
            format!("ffmpeg failed: {detail}")
        });
    }
    let temporary_size = match fs::metadata(&temporary) {
        Ok(metadata) => metadata.len(),
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            return Err(format!("could not inspect extracted audio: {error}"));
        }
    };
    if temporary_size == 0 {
        let _ = fs::remove_file(&temporary);
        return Err("ffmpeg produced an empty audio file".to_owned());
    }
    move_file_no_replace(&temporary, &output).map_err(|error| {
        let cleanup = fs::remove_file(&temporary);
        match cleanup {
            Ok(()) => format!("could not publish audio {}: {error}", output.display()),
            Err(cleanup_error) if cleanup_error.kind() == io::ErrorKind::NotFound => {
                format!("could not publish audio {}: {error}", output.display())
            }
            Err(cleanup_error) => format!(
                "could not publish audio {}: {error}; temporary audio remains at {}: {cleanup_error}",
                output.display(),
                temporary.display()
            ),
        }
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
            "--help" | "-h" => {
                println!(
                    "Usage: mediaforge extract-audio-dir <PATH> --output-dir <DIR> [--extension EXTENSIONS] [--include-hidden] [--format mp3|wav|flac] [--bitrate RATE] [--stream N] [--jobs N] [--skip-existing] [--json]\n\nExtract audio from video and audio assets while preserving relative folders. Defaults to MP3 at 192k from the first audio stream. --jobs controls concurrent ffmpeg processes (1–16; default 4). Existing regular output files are never overwritten; --skip-existing skips those files. Other occupied output paths are errors. The output directory must already exist. --json emits one result record per file as each job completes, with completion counters and a final summary as JSON Lines."
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
    let mut records = Vec::new();
    let kinds = [Kind::Video, Kind::Audio];
    let filters = StrictWalkFilters {
        kinds: Some(&kinds),
        extensions: extensions.as_ref(),
        excluded_subtree: output_inside_root.then_some(output_root.as_path()),
        omit_extension: true,
        ..StrictWalkFilters::default()
    };
    walk_strict_filtered(
        &root,
        &root,
        include_hidden,
        SKIP_DIRS,
        &ignored,
        &filters,
        &mut records,
    )?;
    records.sort_by(|left, right| left.relative.cmp(&right.relative));
    if records.is_empty() {
        if json {
            println!("{{\"type\":\"summary\",\"succeeded\":0,\"skipped\":0,\"failed\":0}}");
        } else {
            println!("No media assets found.");
        }
        return Ok(());
    }

    let total = records.len();
    let mut targets = HashSet::new();
    let plans: Vec<(Option<PathBuf>, Result<(), String>)> = records
        .iter()
        .map(|record| {
            let plan = (|| -> Result<PathBuf, String> {
                let relative = record
                    .path
                    .strip_prefix(&root)
                    .map_err(|error| format!("cannot relativize input path: {error}"))?;
                let stem = relative
                    .file_stem()
                    .and_then(|value| value.to_str())
                    .ok_or_else(|| {
                        "cannot create an audio output name from this path".to_owned()
                    })?;
                let mut relative_output = relative.to_path_buf();
                relative_output.set_file_name(format!("{stem}-audio.{format}"));
                let output = output_root.join(relative_output);
                if !targets.insert(output.clone()) {
                    return Err(format!(
                        "multiple source files map to the same audio path: {}",
                        output.display()
                    ));
                }
                Ok(output)
            })();
            match plan {
                Ok(output) => (Some(output), Ok(())),
                Err(error) => (None, Err(error)),
            }
        })
        .collect();
    let next = std::sync::atomic::AtomicUsize::new(0);
    let completed = std::sync::atomic::AtomicUsize::new(0);
    let progress_output = std::sync::Mutex::new(());
    let succeeded_count = std::sync::atomic::AtomicUsize::new(0);
    let skipped_count = std::sync::atomic::AtomicUsize::new(0);
    let failed_count = std::sync::atomic::AtomicUsize::new(0);
    let results = std::sync::Mutex::new(Vec::<(usize, Result<bool, String>)>::new());
    std::thread::scope(|scope| {
        for _ in 0..jobs.min(total) {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if index >= total {
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
                        let source_text = record
                            .path
                            .to_str()
                            .ok_or_else(|| "input path is not valid Unicode".to_owned())?;
                        let output_text = output
                            .to_str()
                            .ok_or_else(|| "output path is not valid Unicode".to_owned())?;
                        let mut audio_args = vec![
                            source_text.to_owned(),
                            "--output".to_owned(),
                            output_text.to_owned(),
                            "--format".to_owned(),
                            format.clone(),
                            "--stream".to_owned(),
                            stream.to_string(),
                            "--quiet".to_owned(),
                        ];
                        if format == "mp3" {
                            audio_args.push("--bitrate".to_owned());
                            audio_args.push(bitrate.clone());
                        }
                        run_extract_audio(&audio_args).map(|()| true)
                    });
                    if json {
                        let _guard = progress_output
                            .lock()
                            .expect("audio progress output lock poisoned");
                        let current =
                            completed.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                        print_batch_result_json(
                            current,
                            total,
                            &record.relative,
                            output.as_deref(),
                            &result,
                        );
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
                        results
                            .lock()
                            .expect("audio results lock poisoned")
                            .push((index, result));
                    }
                }
            });
        }
    });
    let mut results = results
        .into_inner()
        .map_err(|_| "audio results lock poisoned".to_owned())?;
    let (succeeded, skipped, failed) = if json {
        (
            succeeded_count.load(std::sync::atomic::Ordering::Relaxed),
            skipped_count.load(std::sync::atomic::Ordering::Relaxed),
            failed_count.load(std::sync::atomic::Ordering::Relaxed),
        )
    } else {
        results.sort_by_key(|(index, _)| *index);
        let mut succeeded = 0_usize;
        let mut skipped = 0_usize;
        let mut failed = 0_usize;
        for (index, result) in results.drain(..) {
            let record = &records[index];
            match result {
                Ok(created) => {
                    if created {
                        succeeded += 1;
                    } else {
                        skipped += 1;
                    }
                    if let Some(output) = &plans[index].0 {
                        if created {
                            println!("Created audio {}", output.display());
                        } else {
                            println!("Skipped existing audio {}", output.display());
                        }
                    }
                }
                Err(error) => {
                    failed += 1;
                    eprintln!("mediaforge: {}: {error}", record.relative);
                }
            }
        }
        (succeeded, skipped, failed)
    };
    if json {
        println!(
            "{{\"type\":\"summary\",\"succeeded\":{succeeded},\"skipped\":{skipped},\"failed\":{failed}}}"
        );
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
            let path = entry.path();
            if file_type.is_dir() {
                pending.push(path);
            } else if file_type.is_file() {
                let length = entry
                    .metadata()
                    .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?
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
    if is_real_directory(&root.join("Assets"))
        || is_real_file(&root.join("ProjectSettings/ProjectVersion.txt"))
    {
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
    let root_entries = fs::read_dir(&root)
        .map_err(|error| format!("cannot read project directory {}: {error}", root.display()))?
        .map(|entry| {
            entry.map_err(|error| format!("cannot read entry in {}: {error}", root.display()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut unreal_files = Vec::<(String, Option<String>)>::new();
    for entry in &root_entries {
        let file_type = entry.file_type().map_err(|error| {
            format!(
                "cannot inspect project entry {}: {error}",
                entry.path().display()
            )
        })?;
        if file_type.is_file()
            && entry
                .path()
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("uproject"))
        {
            let project_file = entry.file_name().to_string_lossy().into_owned();
            let contents = fs::read_to_string(entry.path()).map_err(|error| {
                format!(
                    "cannot read Unreal project descriptor {}: {error}",
                    entry.path().display()
                )
            })?;
            let association = json_top_level_string_field(&contents, "EngineAssociation")
                .filter(|association| !association.trim().is_empty());
            unreal_files.push((project_file, association));
        }
    }
    unreal_files.sort_by(|left, right| compare_paths_case_insensitive(&left.0, &right.0));
    if !unreal_files.is_empty() {
        engines.push((
            "Unreal Engine",
            vec!["Content"],
            vec!["Intermediate", "Saved", "DerivedDataCache", "Binaries"],
        ));
    }
    let godot_project_path = root.join("project.godot");
    let godot_features = if is_real_file(&godot_project_path) {
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
    let ignored = ignored_by_git(&root)?;
    let mut pending = vec![(root.clone(), Arc::<[IgnoreRule]>::from(ignored))];
    while let Some((directory, active_ignored)) = pending.pop() {
        let mut entries = fs::read_dir(&directory)
            .map_err(|error| {
                format!(
                    "cannot read project directory {}: {error}",
                    directory.display()
                )
            })?
            .map(|entry| {
                entry.map_err(|error| {
                    format!("cannot read entry in {}: {error}", directory.display())
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        sort_entries(&mut entries);
        for entry in entries {
            let path = entry.path();
            let file_type = entry.file_type().map_err(|error| {
                format!("cannot inspect project entry {}: {error}", path.display())
            })?;
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
                if !directory_name_matches(&lossy_name, SKIP_DIRS)
                    && !directory_name_matches(&lossy_name, PROJECT_ARTIFACT_DIRS)
                    && !is_ignored_directory(&relative, &active_ignored)
                {
                    let child_rules = extend_ignore_rules(&root, &path, active_ignored.as_ref())?;
                    let child_ignored = match child_rules {
                        Cow::Borrowed(_) => Arc::clone(&active_ignored),
                        Cow::Owned(rules) => Arc::from(rules),
                    };
                    pending.push((path, child_ignored));
                }
            } else if file_type.is_file() {
                let relative = path
                    .strip_prefix(&root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                if is_ignored(&relative, &active_ignored) {
                    continue;
                }
                if let Some(kind) = project_file_kind(&relative) {
                    project_files.push((relative, kind));
                }
            }
        }
    }
    project_files.sort_by(|left, right| compare_paths_case_insensitive(&left.0, &right.0));

    let mut missing = Vec::<(&str, &str)>::new();
    let mut caches = Vec::<(String, u64, u64)>::new();
    for (engine, required, cache_names) in &engines {
        for required_path in required {
            let required_entry = root.join(required_path);
            let present = if *required_path == "project.godot" {
                is_real_file(&required_entry)
            } else {
                is_real_directory(&required_entry)
            };
            if !present {
                missing.push((engine, required_path));
            }
        }
        for cache_name in cache_names {
            let cache_path = root.join(cache_name);
            if is_real_directory(&cache_path) {
                let (bytes, files) = directory_size(&cache_path)?;
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
                .map(|(file, _)| json_string(file))
                .collect::<Vec<_>>()
                .join(", ")
        );
        println!("  \"unreal_engine_associations\": [");
        for (index, (file, association)) in unreal_files.iter().enumerate() {
            let comma = if index + 1 == unreal_files.len() {
                ""
            } else {
                ","
            };
            println!(
                "    {{\"project\": {}, \"engine_association\": {}}}{}",
                json_string(file),
                association
                    .as_deref()
                    .map(json_string)
                    .unwrap_or_else(|| "null".to_owned()),
                comma
            );
        }
        println!("  ],");
        println!(
            "  \"godot_features\": [{}],",
            godot_features
                .iter()
                .map(|feature| json_string(feature))
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
    siblings: &HashMap<std::ffi::OsString, (bool, bool)>,
) -> bool {
    let mut sidecar_name = path.file_name().unwrap_or_default().to_os_string();
    sidecar_name.push(".meta");
    if let Some((is_file, _)) = siblings.get(sidecar_name.as_os_str()) {
        return *is_file;
    }
    unity_meta_sidecar_exists(path)
}

fn unity_meta_target(path: &Path) -> Option<PathBuf> {
    let file_name = path.file_name()?.to_string_lossy();
    if !file_name.to_ascii_lowercase().ends_with(".meta") {
        return None;
    }
    let target_name = &file_name[..file_name.len() - ".meta".len()];
    (!target_name.is_empty()).then(|| path.with_file_name(target_name))
}

fn unity_meta_target_exists_in_directory(
    target: &Path,
    siblings: &HashMap<std::ffi::OsString, (bool, bool)>,
) -> bool {
    if let Some((is_file, is_directory)) = target.file_name().and_then(|name| siblings.get(name)) {
        return *is_file || *is_directory;
    }
    is_real_file(target) || is_real_directory(target)
}

fn unity_meta_guid(contents: &str) -> Result<Option<String>, ()> {
    let mut guid = None;
    for (index, raw_line) in contents.lines().enumerate() {
        let line = if index == 0 {
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
            return Err(());
        }
        let value = value.trim();
        let value = match (value.starts_with('"'), value.ends_with('"')) {
            (true, true) if value.len() >= 2 => &value[1..value.len() - 1],
            (false, false) => value,
            _ => return Err(()),
        };
        if value.len() != 32 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(());
        }
        guid = Some(value.to_ascii_lowercase());
    }
    Ok(guid)
}

fn run_unity_meta(args: &[String]) -> Result<(), String> {
    let mut json = false;
    let mut include_hidden = false;
    let mut path = None;
    for arg in args {
        match arg.as_str() {
            "--json" => json = true,
            "--include-hidden" => include_hidden = true,
            "--help" | "-h" => {
                println!(
                    "Usage: mediaforge unity-meta [PROJECT_PATH] [--json] [--include-hidden]\n\nCheck Unity Assets for missing or orphaned .meta files, missing or malformed GUIDs, and duplicate GUID collisions. Honors .gitignore by default, follows no symlinks, and never changes files. Exits with an error status when issues are found."
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
    let mut missing_meta = Vec::<String>::new();
    let mut orphan_meta = Vec::<String>::new();
    let mut missing_guid = Vec::<String>::new();
    let mut invalid_guid = Vec::<String>::new();
    let mut guid_paths = BTreeMap::<String, Vec<String>>::new();
    let assets_meta = unity_meta_sidecar(&assets);
    if !is_real_file(&assets_meta) {
        missing_meta.push("Assets".to_owned());
    } else {
        checked = checked.saturating_add(1);
        let contents = fs::read_to_string(&assets_meta).map_err(|error| {
            format!(
                "cannot read Unity metadata {}: {error}",
                assets_meta.display()
            )
        })?;
        match unity_meta_guid(&contents) {
            Ok(Some(guid)) => guid_paths
                .entry(guid)
                .or_default()
                .push("Assets.meta".to_owned()),
            Ok(None) => missing_guid.push("Assets.meta".to_owned()),
            Err(()) => invalid_guid.push("Assets.meta".to_owned()),
        }
    }
    while let Some((directory, active_rules)) = pending.pop() {
        let mut entries = fs::read_dir(&directory)
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
        sort_entries(&mut entries);
        let mut sibling_types = HashMap::with_capacity(entries.len());
        let mut typed_entries = Vec::with_capacity(entries.len());
        for entry in entries {
            let file_type = entry.file_type().map_err(|error| {
                format!(
                    "cannot inspect Unity asset {}: {error}",
                    entry.path().display()
                )
            })?;
            if file_type.is_file() || file_type.is_dir() {
                sibling_types.insert(entry.file_name(), (file_type.is_file(), file_type.is_dir()));
            }
            typed_entries.push((entry, file_type));
        }
        for (entry, file_type) in typed_entries {
            let entry_path = entry.path();
            if file_type.is_symlink() {
                continue;
            }
            let relative = entry_path
                .strip_prefix(&root)
                .unwrap_or(&entry_path)
                .to_string_lossy()
                .replace('\\', "/");
            if file_type.is_dir() {
                if is_ignored_directory(&relative, &active_rules)
                    || (!include_hidden && is_hidden(&entry_path))
                {
                    continue;
                }
                checked = checked.saturating_add(1);
                if !unity_meta_sidecar_exists_in_directory(&entry_path, &sibling_types) {
                    missing_meta.push(relative);
                }
                let child_rules = extend_ignore_rules(&root, &entry_path, &active_rules)?;
                let child_rules = match child_rules {
                    Cow::Borrowed(_) => Arc::clone(&active_rules),
                    Cow::Owned(rules) => Arc::from(rules),
                };
                pending.push((entry_path, child_rules));
            } else if file_type.is_file() {
                if is_ignored(&relative, &active_rules)
                    || (!include_hidden && is_hidden(&entry_path))
                {
                    continue;
                }
                if let Some(target) = unity_meta_target(&entry_path) {
                    checked = checked.saturating_add(1);
                    if !unity_meta_target_exists_in_directory(&target, &sibling_types) {
                        orphan_meta.push(relative);
                    } else {
                        let contents = fs::read_to_string(&entry_path).map_err(|error| {
                            format!(
                                "cannot read Unity metadata {}: {error}",
                                entry_path.display()
                            )
                        })?;
                        match unity_meta_guid(&contents) {
                            Ok(Some(guid)) => guid_paths.entry(guid).or_default().push(relative),
                            Ok(None) => missing_guid.push(relative),
                            Err(()) => invalid_guid.push(relative),
                        }
                    }
                } else {
                    checked = checked.saturating_add(1);
                    if !unity_meta_sidecar_exists_in_directory(&entry_path, &sibling_types) {
                        missing_meta.push(relative);
                    }
                }
            }
        }
    }
    missing_meta.sort();
    orphan_meta.sort();
    missing_guid.sort();
    invalid_guid.sort();
    let mut duplicate_guids = guid_paths
        .into_iter()
        .filter(|(_, paths)| paths.len() > 1)
        .collect::<Vec<_>>();
    for (_, paths) in &mut duplicate_guids {
        paths.sort();
    }
    let duplicate_guid_count = duplicate_guids
        .iter()
        .map(|(_, paths)| paths.len() - 1)
        .sum::<usize>();
    let issue_count = missing_meta.len()
        + orphan_meta.len()
        + missing_guid.len()
        + invalid_guid.len()
        + duplicate_guid_count;
    if json {
        println!(
            "{{\"root\":{},\"checked\":{checked},\"missing_meta_count\":{},\"orphan_meta_count\":{},\"missing_guid_count\":{},\"invalid_guid_count\":{},\"duplicate_guid_count\":{duplicate_guid_count},\"missing_meta\":[{}],\"orphan_meta\":[{}],\"missing_guid\":[{}],\"invalid_guid\":[{}],\"duplicate_guids\":[{}]}}",
            json_string(&root.to_string_lossy()),
            missing_meta.len(),
            orphan_meta.len(),
            missing_guid.len(),
            invalid_guid.len(),
            missing_meta
                .iter()
                .map(|path| json_string(path))
                .collect::<Vec<_>>()
                .join(","),
            orphan_meta
                .iter()
                .map(|path| json_string(path))
                .collect::<Vec<_>>()
                .join(","),
            missing_guid
                .iter()
                .map(|path| json_string(path))
                .collect::<Vec<_>>()
                .join(","),
            invalid_guid
                .iter()
                .map(|path| json_string(path))
                .collect::<Vec<_>>()
                .join(","),
            duplicate_guids
                .iter()
                .map(|(guid, paths)| {
                    format!(
                        "{{\"guid\":{},\"paths\":[{}]}}",
                        json_string(guid),
                        paths
                            .iter()
                            .map(|path| json_string(path))
                            .collect::<Vec<_>>()
                            .join(",")
                    )
                })
                .collect::<Vec<_>>()
                .join(",")
        );
    } else {
        println!("Unity metadata audit: {}", root.display());
        println!("Checked paths: {checked}");
        println!("Missing .meta files: {}", missing_meta.len());
        for path in &missing_meta {
            println!("  missing: {path}");
        }
        println!("Orphan .meta files: {}", orphan_meta.len());
        for path in &orphan_meta {
            println!("  orphan: {path}");
        }
        println!("Missing GUIDs: {}", missing_guid.len());
        for path in &missing_guid {
            println!("  no guid: {path}");
        }
        println!("Malformed GUIDs: {}", invalid_guid.len());
        for path in &invalid_guid {
            println!("  invalid guid: {path}");
        }
        println!("Duplicate GUID collisions: {duplicate_guid_count}");
        for (guid, paths) in &duplicate_guids {
            println!("  {guid}: {}", paths.join(", "));
        }
        if issue_count == 0 {
            println!("Unity asset metadata and GUIDs are consistent.");
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
    let mut path = None;
    for arg in args {
        match arg.as_str() {
            "--apply" => apply = true,
            "--json" => json = true,
            "--help" | "-h" => {
                println!(
                    "Usage: mediaforge gitignore [PATH] [--apply] [--json]\n\nPreview engine-specific generated-file patterns for .gitignore. Existing files are only appended to when --apply is supplied. --json emits the result and proposed or applied patterns as one object."
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

    let mut entries = fs::read_dir(&root)
        .map_err(|error| format!("cannot read project directory {}: {error}", root.display()))?
        .map(|entry| entry.map_err(|error| format!("cannot read project directory entry: {error}")))
        .collect::<Result<Vec<_>, _>>()?;
    sort_entries(&mut entries);
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
        println!(
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
        );
    };
    if !json {
        println!("Project engines: {}", engines.join(", "));
        println!("Target: {}", gitignore.display());
    }
    if missing.is_empty() {
        if json {
            emit_json("already_current", &[]);
        } else {
            println!("All recommended patterns are already present.");
        }
        return Ok(());
    }
    if !apply {
        if json {
            emit_json("preview", &missing);
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
            emit_json("already_current", &[]);
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
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&gitignore)
                .map_err(|error| format!("cannot create {}: {error}", gitignore.display()))?;
            file.write_all(addition.as_bytes())
                .and_then(|()| file.sync_all())
                .map_err(|error| format!("could not write {}: {error}", gitignore.display()))?;
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
        emit_json("applied", &to_add);
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
            "--help" | "-h" => {
                println!(
                    "Usage: mediaforge organize [PATH] [--by type|extension|size] [--type TYPE] [--extension EXTENSIONS] [--min-size SIZE] [--include-hidden] [--include-generated] [--json] [--apply]\n\nPreview an asset organization plan. --by size groups files into <1 MiB, 1–64 MiB, 64–512 MiB, 512 MiB–2 GiB, and >=2 GiB buckets. --type and --extension select files; --min-size uses bytes or K/M/G/T binary units. Hidden files are skipped unless --include-hidden is set. Unity, Unreal, and Godot generated folders are skipped unless --include-generated is set. --json emits the plan and skipped count as JSON. Files are only moved with --apply; a manifest is written so the move can be undone."
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
    records.sort_by(|left, right| left.relative.cmp(&right.relative));
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
        let destination_state = entry_state(&root.join(&to))
            .map_err(|error| format!("cannot inspect destination {}: {error}", to.display()))?;
        if !destinations.insert(to.clone()) || destination_state != EntryState::Missing {
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

    if json && !apply {
        println!(
            "{{\n  \"root\": {},\n  \"by\": {},\n  \"include_hidden\": {},\n  \"include_generated\": {},\n  \"applied\": {},\n  \"moves\": [",
            json_string(&root.to_string_lossy()),
            json_string(by),
            if include_hidden { "true" } else { "false" },
            if include_generated { "true" } else { "false" },
            if apply { "true" } else { "false" }
        );
        for (index, movement) in moves.iter().enumerate() {
            let comma = if index + 1 == moves.len() { "" } else { "," };
            println!(
                "    {{\"from\": {}, \"to\": {}}}{}",
                json_string(&movement.from),
                json_string(&movement.to),
                comma
            );
        }
        println!(
            "  ],\n  \"move_count\": {},\n  \"skipped_count\": {}\n}}",
            moves.len(),
            skipped
        );
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
            println!(
                "{{\"root\": {}, \"by\": {}, \"include_hidden\": {}, \"include_generated\": {}, \"applied\": {}, \"moves\": [], \"move_count\": 0, \"moved_count\": 0, \"skipped_count\": {}, \"result\": \"no_changes\"}}",
                json_string(&root.to_string_lossy()),
                json_string(by),
                if include_hidden { "true" } else { "false" },
                if include_generated { "true" } else { "false" },
                if apply { "true" } else { "false" },
                skipped
            );
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
        println!(
            "{{\n  \"root\": {},\n  \"by\": {},\n  \"include_hidden\": {},\n  \"include_generated\": {},\n  \"applied\": true,\n  \"moves\": [",
            json_string(&root.to_string_lossy()),
            json_string(by),
            if include_hidden { "true" } else { "false" },
            if include_generated { "true" } else { "false" }
        );
        for (index, movement) in moves.iter().enumerate() {
            let comma = if index + 1 == moves.len() { "" } else { "," };
            println!(
                "    {{\"from\": {}, \"to\": {}}}{}",
                json_string(&movement.from),
                json_string(&movement.to),
                comma
            );
        }
        println!(
            "  ],\n  \"move_count\": {},\n  \"moved_count\": {},\n  \"skipped_count\": {},\n  \"result\": \"applied\",\n  \"undo_manifest\": {}\n}}",
            moves.len(),
            moved,
            skipped,
            json_string(&root.join(ORGANIZE_MANIFEST).to_string_lossy())
        );
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
    for arg in args {
        match arg.as_str() {
            "--json" => json = true,
            "--help" | "-h" => {
                println!(
                    "Usage: mediaforge undo [PATH] [--json]\n\nRestore files moved by the most recent 'organize --apply' or 'sequence --apply' in PATH. --json emits restored and conflict counts."
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
        println!(
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
        );
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
            "--help" | "-h" => {
                println!(
                    "Usage: mediaforge rename [PATH] [--replace OLD NEW] [--prefix TEXT] [--suffix TEXT] [--extension-case keep|lower|upper] [--extension EXTENSIONS] [--include-hidden] [--json] [--apply]\n\nPreview filename changes for files directly inside PATH. Repeated --replace rules run in order; prefix and suffix apply to the filename stem. Matching .meta sidecars are included and moved before their assets to preserve Unity GUIDs. Extensions are preserved unless --extension-case changes their case. The default is preview; --apply renames files and records an undo manifest for 'mediaforge undo PATH'."
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
        if name_text.to_ascii_lowercase().ends_with(".meta") {
            let asset_name = &name_text[..name_text.len() - ".meta".len()];
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
        let extension = source
            .extension()
            .and_then(|value| value.to_str())
            .map(|value| format!(".{}", value.to_ascii_lowercase()))
            .unwrap_or_else(|| "[no extension]".to_owned());
        if extension_filter
            .as_ref()
            .is_some_and(|extensions| !extensions.contains(&extension))
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
        let sidecar_name = format!("{name_text}.meta");
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
        println!(
            "{{\n  \"root\": {},\n  \"applied\": false,\n  \"moves\": [",
            json_string(&root.to_string_lossy())
        );
        for (position, movement) in moves.iter().enumerate() {
            println!(
                "    {{\"from\": {}, \"to\": {}}}{}",
                json_string(&movement.from),
                json_string(&movement.to),
                if position + 1 == moves.len() { "" } else { "," }
            );
        }
        println!(
            "  ],\n  \"move_count\": {},\n  \"skipped_count\": {},\n  \"collision_count\": {collision_count},\n  \"skipped_examples\": [",
            moves.len(),
            skipped
        );
        for (position, (path, reason)) in collision_examples.iter().enumerate() {
            let comma = if position + 1 == collision_examples.len() {
                ""
            } else {
                ","
            };
            println!(
                "    {{\"path\": {}, \"reason\": {}}}{}",
                json_string(path),
                json_string(reason),
                comma
            );
        }
        println!(
            "  ],\n  \"skipped_examples_truncated\": {}\n}}",
            collision_count > collision_examples.len()
        );
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
            println!(
                "{{\"root\":{},\"applied\":true,\"moves\":[],\"move_count\":0,\"renamed_count\":0,\"skipped_count\":{skipped},\"result\":\"no_changes\"}}",
                json_string(&root.to_string_lossy())
            );
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
        println!(
            "{{\"root\":{},\"applied\":true,\"move_count\":{},\"renamed_count\":{renamed},\"skipped_count\":{skipped},\"result\":\"applied\",\"undo_manifest\":{}}}",
            json_string(&root.to_string_lossy()),
            moves.len(),
            json_string(&root.join(ORGANIZE_MANIFEST).to_string_lossy())
        );
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
            "--help" | "-h" => {
                println!(
                    "Usage: mediaforge sequence [PATH] [--prefix NAME] [--start N] [--padding N] [--extension EXTENSIONS] [--include-hidden] [--natural] [--json] [--apply]\n\nPreview a stable renumbering of files directly inside PATH. Alphabetical order is the default; --natural sorts digit runs by numeric value (for example frame2 before frame10). Extensions are preserved. The operation skips destination collisions and can be undone with 'mediaforge undo PATH'."
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
        entries.sort_by(|left, right| {
            let left_name = left.file_name().to_string_lossy().into_owned();
            let right_name = right.file_name().to_string_lossy().into_owned();
            natural_name_cmp(&left_name, &right_name)
        });
    } else {
        sort_entries(&mut entries);
    }
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
        let extension = source
            .extension()
            .and_then(|value| value.to_str())
            .map(|value| format!(".{}", value.to_ascii_lowercase()))
            .unwrap_or_else(|| "[no extension]".to_owned());
        if extension_filter
            .as_ref()
            .is_some_and(|extensions| !extensions.contains(&extension))
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
        let target = root.join(&target_name);
        let state = entry_state(&target)
            .map_err(|error| format!("cannot inspect target {}: {error}", target.display()))?;
        if from_text == to_text || state != EntryState::Missing {
            skipped += 1;
            collision_count += 1;
            if collision_examples.len() < 100 {
                collision_examples.push((
                    from_text,
                    if state == EntryState::Missing {
                        "already_named".to_owned()
                    } else {
                        "destination_exists".to_owned()
                    },
                ));
            }
            continue;
        }
        moves.push(PlannedMove {
            from: from_text,
            to: to_text,
        });
    }

    if json && !apply {
        println!(
            "{{\n  \"root\": {},\n  \"prefix\": {},\n  \"applied\": false,\n  \"moves\": [",
            json_string(&root.to_string_lossy()),
            json_string(&prefix)
        );
        for (position, movement) in moves.iter().enumerate() {
            let comma = if position + 1 == moves.len() { "" } else { "," };
            println!(
                "    {{\"from\": {}, \"to\": {}}}{}",
                json_string(&movement.from),
                json_string(&movement.to),
                comma
            );
        }
        println!(
            "  ],\n  \"move_count\": {},\n  \"skipped_count\": {},\n  \"skipped_examples\": [",
            moves.len(),
            skipped
        );
        for (position, (path, reason)) in collision_examples.iter().enumerate() {
            let comma = if position + 1 == collision_examples.len() {
                ""
            } else {
                ","
            };
            println!(
                "    {{\"path\": {}, \"reason\": {}}}{}",
                json_string(path),
                json_string(reason),
                comma
            );
        }
        println!(
            "  ],\n  \"skipped_examples_truncated\": {}\n}}",
            collision_count > collision_examples.len()
        );
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
            println!(
                "{{\"root\": {}, \"prefix\": {}, \"applied\": true, \"moves\": [], \"move_count\": 0, \"renamed_count\": 0, \"skipped_count\": {}, \"result\": \"no_changes\"}}",
                json_string(&root.to_string_lossy()),
                json_string(&prefix),
                skipped
            );
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
        println!(
            "{{\n  \"root\": {},\n  \"prefix\": {},\n  \"applied\": true,\n  \"moves\": [",
            json_string(&root.to_string_lossy()),
            json_string(&prefix)
        );
        for (position, movement) in moves.iter().enumerate() {
            let comma = if position + 1 == moves.len() { "" } else { "," };
            println!(
                "    {{\"from\": {}, \"to\": {}}}{}",
                json_string(&movement.from),
                json_string(&movement.to),
                comma
            );
        }
        println!(
            "  ],\n  \"move_count\": {},\n  \"renamed_count\": {},\n  \"skipped_count\": {},\n  \"result\": \"applied\",\n  \"undo_manifest\": {}\n}}",
            moves.len(),
            renamed,
            skipped,
            json_string(&root.join(ORGANIZE_MANIFEST).to_string_lossy())
        );
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

fn write_tar_payload(writer: &mut impl Write, contents: &[u8]) -> io::Result<()> {
    writer.write_all(contents)?;
    let padding = (TAR_BLOCK_SIZE - contents.len() % TAR_BLOCK_SIZE) % TAR_BLOCK_SIZE;
    if padding > 0 {
        writer.write_all(&[0_u8; TAR_BLOCK_SIZE][..padding])?;
    }
    Ok(())
}

fn pack_file(
    writer: &mut impl Write,
    archive_name: &str,
    record: &FileRecord,
    index: usize,
    copy_buffer: &mut [u8],
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
    let mut input = File::open(&record.path)
        .map_err(|error| format!("cannot open {}: {error}", record.path.display()))?;
    let initial_metadata = input.metadata().map_err(|error| {
        format!(
            "could not inspect {} before archiving: {error}",
            record.path.display()
        )
    })?;
    if !initial_metadata.is_file()
        || initial_metadata.len() != record.size
        || initial_metadata.modified().ok() != record.modified
    {
        return Err(format!(
            "source changed before packing: {}",
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

    let mut remaining = record.size;
    while remaining > 0 {
        let requested = remaining.min(copy_buffer.len() as u64) as usize;
        let count = loop {
            match input.read(&mut copy_buffer[..requested]) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    return Err(format!("could not read {}: {error}", record.path.display()));
                }
                Ok(count) => break count,
            }
        };
        if count == 0 {
            break;
        }
        writer
            .write_all(&copy_buffer[..count])
            .map_err(|error| format!("could not archive {}: {error}", record.path.display()))?;
        remaining -= count as u64;
    }
    let copied = record.size - remaining;
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
        .trim_end_matches([' ', '.'])
        .to_ascii_uppercase();
    let reserved = matches!(device_name.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ["COM", "LPT"].iter().any(|prefix| {
            device_name.strip_prefix(prefix).is_some_and(|suffix| {
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
    paths: &mut HashMap<String, (String, bool)>,
    relative: &Path,
    display_name: &str,
) -> Result<(), String> {
    let mut components = relative.components().peekable();
    let mut actual_prefix = String::new();
    let mut portable_prefix = String::new();
    while let Some(component) = components.next() {
        let std::path::Component::Normal(component) = component else {
            return Err(format!("unsafe archive path: {}", relative.display()));
        };
        let component = component
            .to_str()
            .ok_or_else(|| format!("non-UTF-8 path cannot be packed: {}", relative.display()))?;
        validate_archive_component(component)?;
        if !actual_prefix.is_empty() {
            actual_prefix.push('/');
            portable_prefix.push('/');
        }
        actual_prefix.push_str(component);
        portable_prefix.push_str(&component.to_lowercase());
        let is_file = components.peek().is_none();
        match paths.get(&portable_prefix) {
            Some((existing, true)) if !is_file => {
                return Err(format!(
                    "archive path uses a file as a directory on case-insensitive filesystems: {existing:?} and {display_name:?}"
                ));
            }
            Some((existing, false)) if is_file => {
                return Err(format!(
                    "archive path uses a directory as a file on case-insensitive filesystems: {existing:?} and {display_name:?}"
                ));
            }
            Some((existing, true)) if is_file => {
                return Err(format!(
                    "archive paths collide on case-insensitive filesystems: {existing:?} and {display_name:?}"
                ));
            }
            Some(_) => {}
            None => {
                paths.insert(portable_prefix.clone(), (actual_prefix.clone(), is_file));
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

fn estimate_tar_size(entries: &[(&FileRecord, &str)]) -> Result<u64, String> {
    let mut total = (TAR_BLOCK_SIZE * 2) as u64;
    for (record, archive_name) in entries {
        let pax_path = ustar_name(archive_name.as_bytes()).is_none();
        let modified = record
            .modified
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
            let pax_size = u64::try_from(pax.len())
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

fn print_pack_json(
    root: &Path,
    output: &Path,
    status: &str,
    dry_run: bool,
    output_exists: bool,
    min_size: u64,
    max_size: Option<u64>,
    exclude_path_contains: &[String],
    files: usize,
    input_bytes: u64,
    estimated_archive_bytes: u64,
    list_entries: bool,
    entries: &[(&FileRecord, &str)],
    archive_bytes: Option<u64>,
) {
    println!("{{");
    println!("  \"root\": {},", json_string(&root.to_string_lossy()));
    println!("  \"output\": {},", json_string(&output.to_string_lossy()));
    println!("  \"status\": {},", json_string(status));
    println!("  \"dry_run\": {dry_run},");
    println!("  \"output_exists\": {output_exists},");
    println!("  \"min_size_bytes\": {min_size},");
    match max_size {
        Some(size) => println!("  \"max_size_bytes\": {size},"),
        None => println!("  \"max_size_bytes\": null,"),
    }
    println!(
        "  \"exclude_path_contains\": [{}],",
        exclude_path_contains
            .iter()
            .map(|value| json_string(value))
            .collect::<Vec<_>>()
            .join(",")
    );
    println!("  \"files\": {files},");
    println!("  \"input_bytes\": {input_bytes},");
    println!("  \"estimated_archive_bytes\": {estimated_archive_bytes},");
    println!(
        "  \"input_size\": {},",
        json_string(&human_size(input_bytes))
    );
    match archive_bytes {
        Some(bytes) => {
            println!("  \"archive_bytes\": {bytes},");
            println!("  \"archive_size\": {},", json_string(&human_size(bytes)));
        }
        None => println!("  \"archive_bytes\": null,\n  \"archive_size\": null,"),
    }
    if list_entries {
        println!("  \"entries\": [");
        for (index, (record, name)) in entries.iter().enumerate() {
            let comma = if index + 1 == entries.len() { "" } else { "," };
            println!(
                "    {{\"path\": {}, \"size_bytes\": {}, \"type\": {}}}{}",
                json_string(name),
                record.size,
                json_string(record.kind.as_str()),
                comma
            );
        }
        println!("  ]");
    } else {
        println!("  \"entries\": null");
    }
    println!("}}");
}

fn run_pack(args: &[String]) -> Result<(), String> {
    let mut path = None;
    let mut output = None;
    let mut include_hidden = false;
    let mut include_generated = false;
    let mut dry_run = false;
    let mut json = false;
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
                    "Usage: mediaforge pack <PATH> --output <FILE.tar> [--type TYPES] [--extension EXTENSIONS] [--min-size SIZE] [--max-size SIZE] [--exclude-path-contains TEXT ...] [--include-hidden] [--include-generated] [--dry-run] [--json] [--list]\n\nCreate an uncompressed, stream-written tar archive. --type accepts comma-separated scan types: video, audio, image, model, texture, project, other. --extension accepts comma-separated extensions such as mov,mxf; use none for files without an extension. --min-size and --max-size accept bytes or K/M/G/T binary units. Repeat --exclude-path-contains to omit matching relative paths; matching directory names prune their subtrees. Git-ignored and hidden files are skipped by default. Common Unity, Unreal, and Godot generated folders are excluded unless --include-generated is used. PAX extensions support long paths and files larger than 8 GiB. --json emits a machine-readable result summary; --list includes per-file paths, sizes, and types in text or JSON output."
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
    let mut records = Vec::new();
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
        omit_modified_time: false,
        omit_extension: true,
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
    let mut entries = Vec::<(&FileRecord, &str)>::with_capacity(records.len());
    let mut portable_names = HashMap::<String, (String, bool)>::new();
    let mut total_bytes = 0_u64;
    for record in &records {
        let relative = record
            .path
            .strip_prefix(&root)
            .map_err(|error| format!("cannot relativize {}: {error}", record.path.display()))?;
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
        register_portable_archive_path(&mut portable_names, relative, &record.relative)?;
        entries.push((record, record.relative.as_str()));
        total_bytes = total_bytes
            .checked_add(record.size)
            .ok_or_else(|| "total input size exceeds the supported range".to_owned())?;
    }
    entries.sort_unstable_by(|left, right| left.1.cmp(right.1));
    let estimated_archive_bytes = estimate_tar_size(&entries)?;
    if !json {
        println!(
            "Pack plan: {} file(s), {} input · estimated archive {}",
            entries.len(),
            human_size(total_bytes),
            human_size(estimated_archive_bytes)
        );
        let displayed_entries = if list_entries {
            entries.len()
        } else {
            entries.len().min(30)
        };
        for (record, name) in entries.iter().take(displayed_entries) {
            println!("  {}  {name}", human_size(record.size));
        }
        if entries.len() > displayed_entries {
            println!("  ... and {} more", entries.len() - displayed_entries);
        }
    }
    if dry_run {
        if json {
            print_pack_json(
                &root,
                &output_path,
                if output_exists {
                    "output_exists"
                } else {
                    "planned"
                },
                true,
                output_exists,
                min_size,
                max_size,
                &exclude_path_contains,
                entries.len(),
                total_bytes,
                estimated_archive_bytes,
                list_entries,
                &entries,
                None,
            );
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

    let mut temp_attempt = 0_u64;
    let temp_path = loop {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let candidate = output_parent.join(format!(
            ".mediaforge-pack-{}-{nonce}-{}.tmp",
            std::process::id(),
            temp_attempt
        ));
        temp_attempt = temp_attempt
            .checked_add(1)
            .ok_or_else(|| "could not allocate a unique temporary archive name".to_owned())?;
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => break (candidate, file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!(
                    "cannot create temporary archive beside {}: {error}",
                    output_path.display()
                ));
            }
        }
    };
    let (temp_path, archive_file) = temp_path;
    let mut archive = BufWriter::with_capacity(64 * 1024, archive_file);
    let mut copy_buffer = vec![0_u8; 256 * 1024];
    let mut archive_bytes = 0_u64;
    let write_result = (|| -> Result<(), String> {
        for (index, (record, name)) in entries.iter().enumerate() {
            pack_file(&mut archive, name, record, index + 1, &mut copy_buffer)?;
        }
        archive
            .write_all(&[0_u8; TAR_BLOCK_SIZE * 2])
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
        return match fs::remove_file(&temp_path) {
            Ok(()) => Err(error),
            Err(cleanup_error) if cleanup_error.kind() == io::ErrorKind::NotFound => Err(error),
            Err(cleanup_error) => Err(format!(
                "{error}; temporary archive could not be removed at {}: {cleanup_error}",
                temp_path.display()
            )),
        };
    }
    if let Err(error) = move_file_no_replace(&temp_path, &output_path) {
        let cleanup = fs::remove_file(&temp_path);
        return Err(match cleanup {
            Ok(()) => format!(
                "could not finalize archive {}: {error}",
                output_path.display()
            ),
            Err(cleanup_error) if cleanup_error.kind() == io::ErrorKind::NotFound => {
                format!(
                    "could not finalize archive {}: {error}",
                    output_path.display()
                )
            }
            Err(cleanup_error) => format!(
                "could not finalize archive {}: {error}; temporary archive remains at {}: {cleanup_error}",
                output_path.display(),
                temp_path.display()
            ),
        });
    }
    let archive_size = archive_bytes;
    if json {
        print_pack_json(
            &root,
            &output_path,
            "created",
            false,
            true,
            min_size,
            max_size,
            &exclude_path_contains,
            entries.len(),
            total_bytes,
            estimated_archive_bytes,
            list_entries,
            &entries,
            Some(archive_size),
        );
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
    for arg in args {
        match arg.as_str() {
            "--json" => json = true,
            "--help" | "-h" => {
                println!(
                    "Usage: mediaforge doctor [--json]\n\nCheck whether ffmpeg and ffprobe are available on PATH, and whether FFmpeg includes the H.264/AAC, MP3, WAV, and FLAC encoders used by MediaForge."
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
    if json {
        let tool_json = |result: &Result<String, String>| match result {
            Ok(version) => format!(
                "{{\"available\":true,\"version\":{}}}",
                json_string(version)
            ),
            Err(error) => format!("{{\"available\":false,\"error\":{}}}", json_string(error)),
        };
        println!(
            "{{\"schema_version\":1,\"type\":\"doctor\",\"media_tools_ready\":{ready},\"proxy_ready\":{proxy_ready},\"audio_encoders\":{{\"mp3\":{},\"wav\":{},\"flac\":{}}},\"ffmpeg\":{},\"ffprobe\":{},\"ffmpeg_encoder_check_error\":{}}}",
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
        );
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
