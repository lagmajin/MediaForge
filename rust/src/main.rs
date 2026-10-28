use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::fmt::Write as FmtWrite;
use std::fs::{self, File, OpenOptions};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{self, BufRead, BufReader, BufWriter, Cursor, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::Arc;

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

fn classify(path: &Path) -> Kind {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "mp4" | "mov" | "mxf" | "mkv" | "avi" | "webm" | "m4v" | "mpg" | "mpeg" | "mpe" | "mpv"
        | "m2v" | "ts" | "mts" | "m2ts" | "vob" | "ogv" | "3gp" | "3g2" | "wmv" | "flv" | "f4v"
        | "asf" | "divx" | "r3d" | "braw" | "crm" => Kind::Video,
        "wav" | "aiff" | "aif" | "flac" | "mp3" | "ogg" | "m4a" | "aac" | "caf" | "ac3"
        | "eac3" | "alac" | "opus" | "wma" | "amr" | "mid" | "midi" => Kind::Audio,
        "png" | "jpg" | "jpeg" | "tif" | "tiff" | "exr" | "hdr" | "tga" | "bmp" | "webp"
        | "gif" | "heic" | "heif" | "jp2" | "j2k" | "dpx" | "cin" | "dng" | "cr2" | "cr3"
        | "nef" | "arw" | "raf" | "orf" | "rw2" | "ari" => Kind::Image,
        "fbx" | "obj" | "gltf" | "glb" | "usd" | "usda" | "usdc" | "usdz" | "abc" | "ma" | "mb"
        | "stl" | "ply" | "3ds" | "dae" => Kind::Model,
        "sbsar" | "psd" | "psb" | "ktx" | "ktx2" | "dds" | "material" | "basis" | "pvr"
        | "astc" => Kind::Texture,
        "uproject" | "uasset" | "umap" | "unity" | "unitypackage" | "asset" | "prefab"
        | "godot" | "tscn" | "tres" | "scn" | "res" | "drp" | "prproj" | "aep" | "fcpxml"
        | "blend" | "sbs" | "spp" => Kind::Project,
        _ => Kind::Other,
    }
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

fn is_hidden(name: &str) -> bool {
    name.starts_with('.')
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

fn sort_entries(entries: &mut [fs::DirEntry]) {
    entries.sort_by_cached_key(|entry| {
        let name = entry.file_name().to_string_lossy().into_owned();
        (name.to_lowercase(), name)
    });
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

fn walk(
    root: &Path,
    current: &Path,
    include_hidden: bool,
    ignored: &[IgnoreRule],
    records: &mut Vec<FileRecord>,
) -> Result<(), String> {
    walk_with_skip_dirs(root, current, include_hidden, SKIP_DIRS, ignored, records)
}

fn walk_with_skip_dirs(
    root: &Path,
    current: &Path,
    include_hidden: bool,
    skip_dirs: &[&str],
    ignored: &[IgnoreRule],
    records: &mut Vec<FileRecord>,
) -> Result<(), String> {
    walk_with_skip_dirs_mode(
        root,
        current,
        include_hidden,
        skip_dirs,
        ignored,
        records,
        false,
    )
}

fn walk_with_skip_dirs_mode(
    root: &Path,
    current: &Path,
    include_hidden: bool,
    skip_dirs: &[&str],
    ignored: &[IgnoreRule],
    records: &mut Vec<FileRecord>,
    strict: bool,
) -> Result<(), String> {
    let mut pending = vec![(current.to_path_buf(), Arc::<[IgnoreRule]>::from(ignored))];
    while let Some((directory, active_ignored)) = pending.pop() {
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(_) if !strict => continue,
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
                Err(_) if !strict => Err(None),
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
            let name = entry.file_name().to_string_lossy().into_owned();
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(_) if !strict => continue,
                Err(error) => return Err(format!("cannot inspect {}: {error}", path.display())),
            };
            if file_type.is_symlink() {
                continue;
            }
            let relative = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            if file_type.is_dir() {
                if directory_name_matches(&name, skip_dirs)
                    || (!include_hidden && is_hidden(&name))
                    || is_ignored_directory(&relative, &active_ignored)
                {
                    continue;
                }
                let child_rules = extend_ignore_rules(root, &path, active_ignored.as_ref())?;
                let child_ignored = match child_rules {
                    Cow::Borrowed(_) => Arc::clone(&active_ignored),
                    Cow::Owned(rules) => Arc::from(rules),
                };
                pending.push((path, child_ignored));
            } else if file_type.is_file()
                && (include_hidden || !is_hidden(&name))
                && !is_ignored(&relative, &active_ignored)
            {
                let metadata = match entry.metadata() {
                    Ok(metadata) => metadata,
                    Err(_) if !strict => continue,
                    Err(error) => {
                        return Err(format!("cannot inspect {}: {error}", path.display()));
                    }
                };
                let extension = path
                    .extension()
                    .and_then(|s| s.to_str())
                    .map(|s| format!(".{}", s.to_ascii_lowercase()))
                    .unwrap_or_else(|| "[no extension]".to_owned());
                records.push(FileRecord {
                    relative,
                    kind: classify(&path),
                    modified: metadata.modified().ok(),
                    size: metadata.len(),
                    extension,
                    path,
                });
            }
        }
    }
    Ok(())
}

fn walk_strict(
    root: &Path,
    current: &Path,
    include_hidden: bool,
    skip_dirs: &[&str],
    ignored: &[IgnoreRule],
    records: &mut Vec<FileRecord>,
) -> Result<(), String> {
    let mut pending = vec![(current.to_path_buf(), Arc::<[IgnoreRule]>::from(ignored))];
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
        sort_entries(&mut entries);
        for entry in entries {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let file_type = entry
                .file_type()
                .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
            if file_type.is_symlink() {
                continue;
            }
            let relative = path
                .strip_prefix(root)
                .map_err(|error| format!("cannot relativize {}: {error}", path.display()))?
                .to_string_lossy()
                .replace('\\', "/");
            if file_type.is_dir() {
                if directory_name_matches(&name, skip_dirs)
                    || (!include_hidden && is_hidden(&name))
                    || is_ignored_directory(&relative, &active_ignored)
                {
                    continue;
                }
                let child_rules = extend_ignore_rules(root, &path, active_ignored.as_ref())?;
                let child_ignored = match child_rules {
                    Cow::Borrowed(_) => Arc::clone(&active_ignored),
                    Cow::Owned(rules) => Arc::from(rules),
                };
                pending.push((path, child_ignored));
            } else if file_type.is_file()
                && (include_hidden || !is_hidden(&name))
                && !is_ignored(&relative, &active_ignored)
            {
                let metadata = entry
                    .metadata()
                    .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
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

fn glob_matches(pattern: &[GlobToken], value: &str) -> bool {
    if pattern
        .iter()
        .all(|token| matches!(token, GlobToken::Literal(_)))
    {
        let mut characters = value.chars();
        return pattern.iter().all(|token| {
            matches!(token, GlobToken::Literal(expected) if Some(*expected) == characters.next())
        }) && characters.next().is_none();
    }
    let value: Vec<char> = value.chars().collect();
    glob_matches_chars(pattern, &value)
}

fn glob_matches_path_prefix(pattern: &[GlobToken], components: &[&str]) -> bool {
    let mut value = Vec::new();
    for (index, component) in components.iter().enumerate() {
        if index > 0 {
            value.push('/');
        }
        value.extend(component.chars());
    }
    glob_matches_chars(pattern, &value)
}

fn glob_matches_chars(pattern: &[GlobToken], value: &[char]) -> bool {
    if pattern
        .iter()
        .all(|token| matches!(token, GlobToken::Literal(_)))
    {
        return pattern.len() == value.len()
            && pattern.iter().zip(value).all(
                |(token, character)| matches!(token, GlobToken::Literal(expected) if expected == character),
            );
    }
    let mut previous = vec![false; value.len() + 1];
    let mut current = vec![false; value.len() + 1];
    let mut directory_prefix = vec![false; value.len() + 1];
    previous[0] = true;
    for token in pattern {
        current.fill(false);
        match token {
            GlobToken::Literal(expected) => {
                for index in 1..=value.len() {
                    current[index] = previous[index - 1] && value[index - 1] == *expected;
                }
            }
            GlobToken::Any => {
                for index in 1..=value.len() {
                    current[index] = previous[index - 1] && value[index - 1] != '/';
                }
            }
            GlobToken::Class { negated, members } => {
                for index in 1..=value.len() {
                    let character = value[index - 1];
                    current[index] = previous[index - 1]
                        && character != '/'
                        && character_class_matches(members, *negated, character);
                }
            }
            GlobToken::Star { crosses_slash } => {
                current[0] = previous[0];
                for index in 1..=value.len() {
                    let character = value[index - 1];
                    current[index] = previous[index]
                        || ((*crosses_slash || character != '/') && current[index - 1]);
                }
            }
            GlobToken::AnyDirectories => {
                current[0] = previous[0];
                directory_prefix[0] = previous[0];
                for index in 1..=value.len() {
                    directory_prefix[index] = previous[index] || directory_prefix[index - 1];
                    current[index] =
                        previous[index] || (value[index - 1] == '/' && directory_prefix[index - 1]);
                }
            }
        }
        std::mem::swap(&mut previous, &mut current);
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

fn rule_matches_path(rule: &IgnoreRule, relative: &str, target_is_directory: bool) -> bool {
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
    let parts: Vec<_> = relative.split('/').collect();
    if !rule.has_slash && !rule.anchored {
        return parts.iter().enumerate().any(|(index, part)| {
            let is_directory = index + 1 < parts.len() || target_is_directory;
            (!rule.directory_only || is_directory) && glob_matches(&rule.pattern, part)
        });
    }
    for end in 1..=parts.len() {
        let is_directory = end < parts.len() || target_is_directory;
        if rule.directory_only && !is_directory {
            continue;
        }
        if glob_matches_path_prefix(&rule.pattern, &parts[..end]) {
            return true;
        }
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
    let mut ignored = false;
    for rule in rules {
        if rule_matches_path(rule, relative, is_directory) {
            ignored = !rule.negated;
        }
    }
    ignored
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

fn print_json(root: &Path, records: &[FileRecord], largest_limit: usize) {
    let mut by_type: BTreeMap<&str, (u64, u128)> = BTreeMap::new();
    let mut extensions: BTreeMap<&str, usize> = BTreeMap::new();
    let mut extension_bytes: BTreeMap<&str, u128> = BTreeMap::new();
    let mut by_size: HashMap<u64, Vec<&str>> = HashMap::new();
    let total_bytes: u128 = records.iter().map(|record| u128::from(record.size)).sum();
    for record in records {
        let entry = by_type.entry(record.kind.as_str()).or_default();
        entry.0 += 1;
        entry.1 += u128::from(record.size);
        *extensions.entry(&record.extension).or_default() += 1;
        *extension_bytes.entry(&record.extension).or_default() += u128::from(record.size);
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
    println!("  \"largest_limit\": {largest_limit},");
    println!("  \"total_bytes\": {},", total_bytes);
    println!(
        "  \"total_size\": {},",
        json_string(&human_size_wide(total_bytes))
    );
    println!("  \"by_type\": {{");
    for (index, (kind, (count, bytes))) in by_type.iter().enumerate() {
        let comma = if index + 1 == by_type.len() { "" } else { "," };
        println!(
            "    {}: {{\"files\": {}, \"bytes\": {}, \"size\": {}}}{}",
            json_string(kind),
            count,
            bytes,
            json_string(&human_size_wide(*bytes)),
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
    println!("  }},\n  \"extension_bytes\": {{");
    for (index, (extension, bytes)) in extension_bytes.iter().enumerate() {
        let comma = if index + 1 == extension_bytes.len() {
            ""
        } else {
            ","
        };
        println!("    {}: {}{}", json_string(extension), bytes, comma);
    }
    println!("  }},\n  \"largest\": [");
    for (index, record) in largest.iter().take(largest_limit).enumerate() {
        let comma = if index + 1 == largest.len().min(largest_limit) {
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

fn print_report(root: &Path, records: &[FileRecord], largest_limit: usize) {
    let total_bytes: u128 = records.iter().map(|record| u128::from(record.size)).sum();
    let mut by_type: BTreeMap<&str, (u64, u128)> = BTreeMap::new();
    let mut by_size: HashMap<u64, usize> = HashMap::new();
    for record in records {
        let entry = by_type.entry(record.kind.as_str()).or_default();
        entry.0 += 1;
        entry.1 += u128::from(record.size);
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
    println!("{} files · {}", records.len(), human_size_wide(total_bytes));
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
    for record in largest.iter().take(largest_limit) {
        println!("  {:>10}  {}", human_size(record.size), record.relative);
    }
    println!("\nSame-size candidate groups: {candidate_groups} (not content-verified)");
}

fn csv_field(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('"');
    for character in value.chars() {
        if character == '"' {
            escaped.push('"');
        }
        escaped.push(character);
    }
    escaped.push('"');
    escaped
}

fn print_csv(records: &[FileRecord]) -> Result<(), String> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    output
        .write_all(b"path,type,extension,size_bytes,modified_unix_seconds\n")
        .map_err(|error| format!("could not write CSV header: {error}"))?;
    for record in records {
        let modified = record
            .modified
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs().to_string())
            .unwrap_or_default();
        let row = format!(
            "{},{},{},{},{}\n",
            csv_field(&record.relative),
            csv_field(record.kind.as_str()),
            csv_field(&record.extension),
            record.size,
            modified
        );
        output
            .write_all(row.as_bytes())
            .map_err(|error| format!("could not write CSV row: {error}"))?;
    }
    Ok(())
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

fn sample_signature(path: &Path, size: u64) -> io::Result<u64> {
    const SAMPLE_SIZE: usize = 4096;
    let mut file = fs::File::open(path)?;
    let mut offsets = vec![0_u64];
    if size > SAMPLE_SIZE as u64 {
        offsets.push(size / 2 - (SAMPLE_SIZE as u64 / 2));
        offsets.push(size.saturating_sub(SAMPLE_SIZE as u64));
    }
    offsets.sort_unstable();
    offsets.dedup();
    let mut buffer = vec![0_u8; SAMPLE_SIZE];
    let mut hasher = DefaultHasher::new();
    size.hash(&mut hasher);
    for offset in offsets {
        file.seek(io::SeekFrom::Start(offset))?;
        let count = (size - offset).min(SAMPLE_SIZE as u64) as usize;
        file.read_exact(&mut buffer[..count])?;
        offset.hash(&mut hasher);
        count.hash(&mut hasher);
        buffer[..count].hash(&mut hasher);
    }
    Ok(hasher.finish())
}

fn run_dupes(args: &[String]) -> Result<(), String> {
    let mut json = false;
    let mut include_hidden = false;
    let mut min_size = 0;
    let mut max_size = None;
    let mut kind_filter = None;
    let mut extension_filter = None;
    let mut top_groups = None;
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
            "--help" | "-h" => {
                println!(
                    "Usage: mediaforge dupes [PATH] [--json] [--include-hidden] [--type TYPE] [--min-size SIZE] [--max-size SIZE] [--extension EXTENSIONS] [--top-groups N]\n\nFind byte-for-byte duplicate files. Files are compared in fixed-size chunks to keep memory use low. --min-size and --max-size limit which file sizes are checked; both accept bytes or K/M/G/T units. --top-groups limits displayed groups, ordered by reclaimable space; summary totals include all groups. --type accepts video, audio, image, model, texture, project, or other. --extension accepts a comma-separated list such as mov,mxf; use none for files without an extension. Fractional size thresholds round up to the next byte."
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
    walk_strict(
        &root,
        &root,
        include_hidden,
        SKIP_DIRS,
        &ignored,
        &mut records,
    )?;
    if let Some(kind) = kind_filter {
        records.retain(|record| record.kind == kind);
    }
    if let Some(extensions) = &extension_filter {
        records.retain(|record| extensions.contains(&record.extension));
    }
    let mut by_size: BTreeMap<u64, Vec<String>> = BTreeMap::new();
    for record in records.into_iter().filter(|record| {
        record.size >= min_size && max_size.map_or(true, |max_size| record.size <= max_size)
    }) {
        by_size
            .entry(record.size)
            .or_default()
            .push(record.relative);
    }

    let mut groups: Vec<(u64, Vec<String>)> = Vec::new();
    for (size, paths) in by_size.into_iter().filter(|(_, paths)| paths.len() > 1) {
        let mut by_sample: HashMap<u64, Vec<Vec<String>>> = HashMap::new();
        for relative in paths {
            let candidate = root.join(&relative);
            let signature = sample_signature(&candidate, size)
                .map_err(|error| format!("could not sample {}: {error}", candidate.display()))?;
            let candidates = by_sample.entry(signature).or_default();
            let mut matched = false;
            for group in candidates.iter_mut() {
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
        for group in by_sample
            .into_values()
            .flatten()
            .filter(|group| group.len() > 1)
        {
            groups.push((size, group));
        }
    }
    groups.sort_by(|left, right| {
        (u128::from(right.0) * right.1.len().saturating_sub(1) as u128)
            .cmp(&(u128::from(left.0) * left.1.len().saturating_sub(1) as u128))
            .then_with(|| right.0.cmp(&left.0))
            .then_with(|| left.1.first().cmp(&right.1.first()))
    });

    let total_groups = groups.len();
    let duplicate_files: usize = groups.iter().map(|(_, files)| files.len()).sum();
    let reclaimable: u128 = groups
        .iter()
        .map(|(size, files)| u128::from(*size) * files.len().saturating_sub(1) as u128)
        .sum();
    let displayed_groups = top_groups.unwrap_or(total_groups).min(total_groups);
    let truncated_groups = total_groups - displayed_groups;
    if json {
        println!(
            "{{\n  \"root\": {},\n  \"min_size_bytes\": {},\n  \"max_size_bytes\": {},\n  \"groups_total\": {},\n  \"groups_returned\": {},\n  \"groups_truncated\": {},\n  \"groups\": [",
            json_string(&root.to_string_lossy()),
            min_size,
            max_size.map_or_else(|| "null".to_owned(), |size| size.to_string()),
            total_groups,
            displayed_groups,
            truncated_groups
        );
        for (index, (size, files)) in groups.iter().take(displayed_groups).enumerate() {
            let comma = if index + 1 == displayed_groups {
                ""
            } else {
                ","
            };
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
    let mut json = false;
    let mut csv = false;
    let mut include_hidden = false;
    let mut path = None;
    let mut kind_filter = None;
    let mut min_size = None;
    let mut extension_filter = None;
    let mut largest_limit = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--json" => json = true,
            "--csv" => csv = true,
            "--include-hidden" => include_hidden = true,
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
                extension_filter = Some(parse_extensions(value)?);
            }
            "--help" | "-h" => {
                println!(
                    "Usage: mediaforge scan [PATH] [--json|--csv] [--include-hidden] [--type TYPE] [--min-size SIZE] [--extension EXTENSIONS] [--largest N]\n\nSummarize media and game assets in a folder. --csv emits one row per matched file with path, type, extension, byte size, and modification time in Unix seconds; it always includes all matches. --largest controls how many biggest files are shown (default: 10 in text, 20 in JSON). Defaults to the current directory.\n\nTypes: video, audio, image, model, texture, project, other\nExtensions: comma-separated, case-insensitive (for example exr,tif); use none for files without an extension\nSizes: bytes, KiB/MiB/GiB, or K/M/G (for example 500M); fractional values round up to the next byte"
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
    let path = path.unwrap_or_else(|| PathBuf::from("."));
    let root =
        fs::canonicalize(&path).map_err(|e| format!("cannot access {}: {e}", path.display()))?;
    if !root.is_dir() {
        return Err(format!("expected a directory: {}", root.display()));
    }
    let ignored = ignored_by_git(&root)?;
    let mut records = Vec::new();
    walk(&root, &root, include_hidden, &ignored, &mut records)?;
    if let Some(kind) = kind_filter {
        records.retain(|record| record.kind == kind);
    }
    if let Some(extensions) = extension_filter {
        records.retain(|record| extensions.contains(&record.extension));
    }
    if let Some(size) = min_size {
        records.retain(|record| record.size >= size);
    }
    let largest_limit = largest_limit.unwrap_or(if json { 20 } else { 10 });
    if csv {
        print_csv(&records)?;
    } else if json {
        print_json(&root, &records, largest_limit);
    } else {
        print_report(&root, &records, largest_limit);
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
        "MediaForge {VERSION}\nFast, local-first CLI for game and video production assets.\n\nUsage:\n  mediaforge scan [PATH] [--json|--csv] [--include-hidden] [--type TYPE] [--min-size SIZE] [--extension EXTENSIONS] [--largest N]\n  mediaforge dupes [PATH] [--json] [--include-hidden] [--type TYPE] [--min-size SIZE] [--max-size SIZE] [--extension EXTENSIONS] [--top-groups N]\n  mediaforge probe <FILE> [--compact] [--summary]\n  mediaforge probe-dir <PATH> [--extension EXTENSIONS] [--include-hidden] [--type video|audio] [--min-size SIZE] [--max-size SIZE] [--jobs N] [--summary] [--compare]\n  mediaforge storyboard <VIDEO> --output <IMAGE> [--interval SECONDS] [--columns N] [--rows N] [--width PIXELS]\n  mediaforge proxy <VIDEO> --output <FILE.mp4> [--profile fast|balanced|quality] [--settings FILE] [--max-width PIXELS] [--crf N] [--preset PRESET]\n  mediaforge proxy-dir <PATH> --output-dir <DIR> [--extension EXTENSIONS] [--include-hidden] [--profile fast|balanced|quality] [--settings FILE] [--max-width PIXELS] [--crf N] [--preset PRESET] [--jobs N] [--skip-existing] [--json]\n  mediaforge proxy-config init [FILE] [--profile fast|balanced|quality] [--max-width PIXELS] [--crf N] [--preset PRESET]\n  mediaforge extract-audio <VIDEO> --output <FILE> [--format mp3|wav|flac] [--bitrate RATE] [--stream N]\n  mediaforge extract-audio-dir <PATH> --output-dir <DIR> [--extension EXTENSIONS] [--include-hidden] [--format mp3|wav|flac] [--bitrate RATE] [--stream N] [--jobs N] [--skip-existing] [--json]\n  mediaforge project [PATH] [--json]\n  mediaforge gitignore [PATH] [--apply]\n  mediaforge organize [PATH] [--by type|extension] [--type TYPE] [--extension EXTENSIONS] [--min-size SIZE] [--include-hidden] [--include-generated] [--json] [--apply]\n  mediaforge sequence [PATH] [--prefix NAME] [--start N] [--padding N] [--extension EXTENSIONS] [--include-hidden] [--natural] [--json] [--apply]\n  mediaforge undo [PATH] [--json]\n  mediaforge pack <PATH> --output <FILE.tar> [--type TYPES] [--extension EXTENSIONS] [--min-size SIZE] [--max-size SIZE] [--include-hidden] [--include-generated] [--dry-run] [--json] [--list]\n  mediaforge --version\n  mediaforge --help"
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
    write!(
        output,
        "{{\"files\":{},\"successful\":{successful},\"varying_fields\":[",
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
        write!(
            output,
            "{{\"path\":{},\"result\":{result}}}",
            json_string(&records[*index].relative)
        )
        .map_err(|error| format!("could not format comparison: {error}"))?;
    }
    output.push_str("]}");
    println!("{output}");
    Ok(())
}

fn run_probe_dir(args: &[String]) -> Result<(), String> {
    const MAX_PROBE_JOBS: usize = 32;
    let mut include_hidden = false;
    let mut summary = false;
    let mut compare = false;
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
                    "Usage: mediaforge probe-dir <PATH> [--extension EXTENSIONS] [--include-hidden] [--type video|audio] [--min-size SIZE] [--max-size SIZE] [--jobs N] [--summary] [--compare]\n\nProbe video and audio assets in a directory with ffprobe. Emits one JSON Lines record per file. --type filters to video or audio assets and selects only that stream type in the report. --min-size and --max-size filter file sizes before probing and accept bytes or K/M/G/T units. --jobs controls concurrent ffprobe processes (1–32). --summary selects compact fields. --compare implies --summary and emits only first-stream media-spec fields that differ across files, grouped by value. Use --type to compare video or audio streams consistently."
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
    walk_strict(
        &root,
        &root,
        include_hidden,
        SKIP_DIRS,
        &ignored,
        &mut records,
    )?;
    records.retain(|record| matches!(record.kind, Kind::Video | Kind::Audio));
    if let Some(kind) = selected_kind {
        records.retain(|record| record.kind == kind);
    }
    if let Some(extensions) = extensions {
        records.retain(|record| extensions.contains(&record.extension));
    }
    records
        .retain(|record| record.size >= min_size && max_size.is_none_or(|max| record.size <= max));
    records.sort_by(|left, right| left.relative.cmp(&right.relative));
    if records.is_empty() {
        if compare {
            println!("{{\"files\":0,\"successful\":0,\"varying_fields\":[],\"failures\":[]}}");
        }
        return Ok(());
    }

    let next = std::sync::atomic::AtomicUsize::new(0);
    let results = std::sync::Mutex::new(
        Vec::<(usize, bool, String, Option<String>)>::with_capacity(records.len()),
    );
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
                    let result = ffprobe_json(&record.path, false, summary, selector);
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
                    results
                        .lock()
                        .expect("probe result mutex poisoned")
                        .push((index, success, json, probe));
                }
            });
        }
    });
    let mut results = results
        .into_inner()
        .map_err(|_| "probe results could not be collected".to_owned())?;
    results.sort_by_key(|(index, _, _, _)| *index);
    let failures = results.iter().filter(|(_, success, _, _)| !success).count();
    if compare {
        summarize_probe_differences(&records, &results)?;
    } else {
        let stdout = io::stdout();
        let mut stdout = stdout.lock();
        for (_, _, record, _) in &results {
            writeln!(stdout, "{record}")
                .map_err(|error| format!("could not write probe results: {error}"))?;
        }
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
                    "Usage: mediaforge proxy-dir <PATH> --output-dir <DIR> [--extension EXTENSIONS] [--include-hidden] [--profile fast|balanced|quality] [--settings FILE] [--max-width PIXELS] [--crf N] [--preset PRESET] [--jobs N] [--skip-existing] [--json]\n\nCreate H.264/AAC proxies for video assets while preserving their relative folders. Automatically reads .mediaforge-proxy from the working directory when present. Explicit options override settings-file values. --jobs controls concurrent ffmpeg processes (1–16; default 4). Existing outputs are never overwritten; --skip-existing skips them to resume a batch. Failed files are reported after the remaining videos are processed. The output directory must already exist. --json emits progress and per-file result records as JSON Lines."
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
    walk_strict(
        &root,
        &root,
        include_hidden,
        SKIP_DIRS,
        &ignored,
        &mut records,
    )?;
    records.retain(|record| record.kind == Kind::Video);
    if let Some(extensions) = extensions {
        records.retain(|record| extensions.contains(&record.extension));
    }
    records.sort_by(|left, right| left.relative.cmp(&right.relative));
    if output_inside_root {
        records.retain(|record| !record.path.starts_with(&output_root));
    }
    if records.is_empty() {
        if json {
            println!("{{\"type\":\"summary\",\"succeeded\":0,\"failed\":0}}");
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
    let results = std::sync::Mutex::new(Vec::<(usize, Result<bool, String>)>::with_capacity(total));
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
                        if skip_existing && output_state != EntryState::Missing {
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
                        let current = completed.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                            + 1;
                        println!(
                            "{{\"type\":\"progress\",\"current\":{current},\"total\":{},\"path\":{}}}",
                            total,
                            json_string(&record.relative)
                        );
                    }
                    results
                        .lock()
                        .expect("proxy results lock poisoned")
                        .push((index, result));
                }
            });
        }
    });
    let mut results = results
        .into_inner()
        .map_err(|_| "proxy results lock poisoned".to_owned())?;
    results.sort_by_key(|(index, _)| *index);
    let mut succeeded = 0_usize;
    let mut skipped = 0_usize;
    let mut failed = 0_usize;
    for (index, result) in results {
        let record = &records[index];
        let output_json = plans[index]
            .0
            .as_ref()
            .map(|output| json_string(&output.to_string_lossy()))
            .unwrap_or_else(|| "null".to_owned());
        match result {
            Ok(created) => {
                if created {
                    succeeded += 1;
                } else {
                    skipped += 1;
                }
                if json {
                    if created {
                        println!(
                            "{{\"type\":\"result\",\"path\":{},\"output\":{},\"ok\":true}}",
                            json_string(&record.relative),
                            output_json
                        );
                    } else {
                        println!(
                            "{{\"type\":\"result\",\"path\":{},\"output\":{},\"ok\":true,\"skipped\":true}}",
                            json_string(&record.relative),
                            output_json
                        );
                    }
                } else if let Some(output) = &plans[index].0 {
                    if created {
                        println!("Created proxy {}", output.display());
                    } else {
                        println!("Skipped existing proxy {}", output.display());
                    }
                }
            }
            Err(error) => {
                failed += 1;
                if json {
                    println!(
                        "{{\"type\":\"result\",\"path\":{},\"output\":{},\"ok\":false,\"error\":{}}}",
                        json_string(&record.relative),
                        output_json,
                        json_string(&error)
                    );
                } else {
                    eprintln!("mediaforge: {}: {error}", record.relative);
                }
            }
        }
    }
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
                    "Usage: mediaforge extract-audio-dir <PATH> --output-dir <DIR> [--extension EXTENSIONS] [--include-hidden] [--format mp3|wav|flac] [--bitrate RATE] [--stream N] [--jobs N] [--skip-existing] [--json]\n\nExtract audio from video and audio assets while preserving relative folders. Defaults to MP3 at 192k from the first audio stream. --jobs controls concurrent ffmpeg processes (1–16; default 4). Existing outputs are never overwritten; --skip-existing skips them to resume a batch. The output directory must already exist. --json emits progress and per-file results as JSON Lines."
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
    walk_strict(
        &root,
        &root,
        include_hidden,
        SKIP_DIRS,
        &ignored,
        &mut records,
    )?;
    records.retain(|record| matches!(record.kind, Kind::Video | Kind::Audio));
    if let Some(extensions) = extensions {
        records.retain(|record| extensions.contains(&record.extension));
    }
    records.sort_by(|left, right| left.relative.cmp(&right.relative));
    if output_inside_root {
        records.retain(|record| !record.path.starts_with(&output_root));
    }
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
    let results = std::sync::Mutex::new(Vec::<(usize, Result<bool, String>)>::with_capacity(total));
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
                        if skip_existing && output_state != EntryState::Missing {
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
                        let current = completed.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                            + 1;
                        println!(
                            "{{\"type\":\"progress\",\"current\":{current},\"total\":{},\"path\":{}}}",
                            total,
                            json_string(&record.relative)
                        );
                    }
                    results
                        .lock()
                        .expect("audio results lock poisoned")
                        .push((index, result));
                }
            });
        }
    });
    let mut results = results
        .into_inner()
        .map_err(|_| "audio results lock poisoned".to_owned())?;
    results.sort_by_key(|(index, _)| *index);
    let mut succeeded = 0_usize;
    let mut skipped = 0_usize;
    let mut failed = 0_usize;
    for (index, result) in results {
        let record = &records[index];
        let output_json = plans[index]
            .0
            .as_ref()
            .map(|output| json_string(&output.to_string_lossy()))
            .unwrap_or_else(|| "null".to_owned());
        match result {
            Ok(created) => {
                if created {
                    succeeded += 1;
                } else {
                    skipped += 1;
                }
                if json {
                    if created {
                        println!(
                            "{{\"type\":\"result\",\"path\":{},\"output\":{},\"ok\":true}}",
                            json_string(&record.relative),
                            output_json
                        );
                    } else {
                        println!(
                            "{{\"type\":\"result\",\"path\":{},\"output\":{},\"ok\":true,\"skipped\":true}}",
                            json_string(&record.relative),
                            output_json
                        );
                    }
                } else if let Some(output) = &plans[index].0 {
                    if created {
                        println!("Created audio {}", output.display());
                    } else {
                        println!("Skipped existing audio {}", output.display());
                    }
                }
            }
            Err(error) => {
                failed += 1;
                if json {
                    println!(
                        "{{\"type\":\"result\",\"path\":{},\"output\":{},\"ok\":false,\"error\":{}}}",
                        json_string(&record.relative),
                        output_json,
                        json_string(&error)
                    );
                } else {
                    eprintln!("mediaforge: {}: {error}", record.relative);
                }
            }
        }
    }
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
    let root = root.to_path_buf();
    let ignored = ignored_by_git(&root)?;
    let mut pending = vec![(root.clone(), Arc::<[IgnoreRule]>::from(ignored))];
    let mut bytes = 0_u64;
    let mut files = 0_u64;
    while let Some((directory, active_ignored)) = pending.pop() {
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
            let relative = path
                .strip_prefix(&root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            if file_type.is_dir() {
                let name = entry.file_name();
                if !directory_name_matches(&name.to_string_lossy(), SKIP_DIRS)
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
                if !is_ignored(&relative, &active_ignored) {
                    let length = entry
                        .metadata()
                        .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?
                        .len();
                    bytes = bytes.saturating_add(length);
                    files = files.saturating_add(1);
                }
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
        if section == "application" {
            if let Some((key, value)) = line.split_once('=') {
                if key.trim() == "config/features" {
                    return parse_godot_string_array(value.trim());
                }
            }
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
    unreal_files.sort_by(|left, right| {
        left.0
            .to_lowercase()
            .cmp(&right.0.to_lowercase())
            .then_with(|| left.0.cmp(&right.0))
    });
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
    project_files.sort_by(|left, right| {
        left.0
            .to_lowercase()
            .cmp(&right.0.to_lowercase())
            .then_with(|| left.0.cmp(&right.0))
    });

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
    let mut path = None;
    for arg in args {
        match arg.as_str() {
            "--apply" => apply = true,
            "--help" | "-h" => {
                println!(
                    "Usage: mediaforge gitignore [PATH] [--apply]\n\nPreview engine-specific generated-file patterns for .gitignore. Existing files are only appended to when --apply is supplied."
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

    println!("Project engines: {}", engines.join(", "));
    println!("Target: {}", gitignore.display());
    if missing.is_empty() {
        println!("All recommended patterns are already present.");
        return Ok(());
    }
    println!("{} new pattern(s):", missing.len());
    for pattern in &missing {
        println!("  {pattern}");
    }
    if unreal {
        println!(
            "Note: Unreal's Saved/ folder is not ignored because it can contain backups and logs."
        );
    }
    if !apply {
        println!("Preview only. Re-run with --apply to append these patterns.");
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
        println!("No changes needed; the patterns were added by another process.");
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
    println!(
        "Appended {} pattern(s) to {}.",
        to_add.len(),
        gitignore.display()
    );
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
                    .ok_or_else(|| "--by requires 'type' or 'extension'".to_owned())?;
                if by != "type" && by != "extension" {
                    return Err("--by must be 'type' or 'extension'".to_owned());
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
                    "Usage: mediaforge organize [PATH] [--by type|extension] [--type TYPE] [--extension EXTENSIONS] [--min-size SIZE] [--include-hidden] [--include-generated] [--json] [--apply]\n\nPreview an asset organization plan. --type and --extension select files; --min-size uses bytes or K/M/G/T binary units. Hidden files are skipped unless --include-hidden is set. Unity, Unreal, and Godot generated folders are skipped unless --include-generated is set. --json emits the plan and skipped count as JSON. Files are only moved with --apply; a manifest is written so the move can be undone."
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
    walk_strict(
        &root,
        &root,
        include_hidden,
        &skip_dirs,
        &ignored,
        &mut records,
    )?;
    records.sort_by(|left, right| left.relative.cmp(&right.relative));
    let mut moves = Vec::<PlannedMove>::new();
    let mut skipped = 0usize;
    let mut destinations = HashSet::<PathBuf>::new();
    for record in records {
        if kind_filter.is_some_and(|kind| record.kind != kind)
            || extension_filter
                .as_ref()
                .is_some_and(|extensions| !extensions.contains(&record.extension))
            || record.size < min_size
        {
            continue;
        }
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
        if !moves.is_empty() {
            if !json {
                println!(
                    "\nPreview only. Re-run with --apply to move files; then use 'mediaforge undo {}' to restore them.",
                    root.display()
                );
            }
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
    let mut index = 0_u64;
    for entry in entries {
        let source = entry.path();
        let name = entry.file_name();
        let Some(name_text) = name.to_str() else {
            skipped += 1;
            continue;
        };
        if (!include_hidden && is_hidden(name_text))
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
            "  ],\n  \"move_count\": {},\n  \"skipped_count\": {}\n}}",
            moves.len(),
            skipped
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
        .trim_end_matches(|character| character == ' ' || character == '.')
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
    archive_name: &str,
) -> Result<(), String> {
    let components: Vec<_> = archive_name.split('/').collect();
    let mut actual_prefix = String::new();
    let mut portable_prefix = String::new();
    for (index, component) in components.iter().enumerate() {
        if !actual_prefix.is_empty() {
            actual_prefix.push('/');
            portable_prefix.push('/');
        }
        actual_prefix.push_str(component);
        portable_prefix.push_str(&component.to_lowercase());
        let is_file = index + 1 == components.len();
        match paths.get(&portable_prefix) {
            Some((existing, true)) if !is_file => {
                return Err(format!(
                    "archive path uses a file as a directory on case-insensitive filesystems: {existing:?} and {archive_name:?}"
                ));
            }
            Some((existing, false)) if is_file => {
                return Err(format!(
                    "archive path uses a directory as a file on case-insensitive filesystems: {existing:?} and {archive_name:?}"
                ));
            }
            Some((existing, true)) if is_file => {
                return Err(format!(
                    "archive paths collide on case-insensitive filesystems: {existing:?} and {archive_name:?}"
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

fn estimate_tar_size(entries: &[(&FileRecord, String)]) -> Result<u64, String> {
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
    files: usize,
    input_bytes: u64,
    estimated_archive_bytes: u64,
    list_entries: bool,
    entries: &[(&FileRecord, String)],
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
                    "Usage: mediaforge pack <PATH> --output <FILE.tar> [--type TYPES] [--extension EXTENSIONS] [--min-size SIZE] [--max-size SIZE] [--include-hidden] [--include-generated] [--dry-run] [--json] [--list]\n\nCreate an uncompressed, stream-written tar archive. --type accepts comma-separated scan types: video, audio, image, model, texture, project, other. --extension accepts comma-separated extensions such as mov,mxf; use none for files without an extension. --min-size and --max-size accept bytes or K/M/G/T binary units. Git-ignored and hidden files are skipped by default. Common Unity, Unreal, and Godot generated folders are excluded unless --include-generated is used. PAX extensions support long paths and files larger than 8 GiB. --json emits a machine-readable result summary; --list includes per-file paths, sizes, and types in text or JSON output."
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
    walk_strict(
        &root,
        &root,
        include_hidden,
        &skip_dirs,
        &ignored,
        &mut records,
    )?;
    if let Some(types) = &selected_types {
        records.retain(|record| types.contains(&record.kind));
    }
    if let Some(extensions) = &selected_extensions {
        records.retain(|record| extensions.contains(&record.extension));
    }
    records.retain(|record| record.size >= min_size);
    if let Some(max_size) = max_size {
        records.retain(|record| record.size <= max_size);
    }
    let mut entries = Vec::<(&FileRecord, String)>::with_capacity(records.len());
    let mut portable_names = HashMap::<String, (String, bool)>::new();
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
            validate_archive_component(part)?;
            components.push(part);
        }
        if components.is_empty() {
            return Err("cannot archive the input directory itself".to_owned());
        }
        let archive_name = components.join("/");
        if archive_name.len() > 1024 * 1024 {
            return Err(format!("archive path is too long: {archive_name}"));
        }
        register_portable_archive_path(&mut portable_names, &archive_name)?;
        entries.push((record, archive_name));
        total_bytes = total_bytes
            .checked_add(record.size)
            .ok_or_else(|| "total input size exceeds the supported range".to_owned())?;
    }
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
            .map_err(|error| format!("could not persist archive: {error}"))
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
    let archive_size = fs::metadata(&output_path)
        .map_err(|error| format!("archive was created but could not be inspected: {error}"))?
        .len();
    if json {
        print_pack_json(
            &root,
            &output_path,
            "created",
            false,
            true,
            min_size,
            max_size,
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
        Some("probe-dir") => run_probe_dir(&args[1..]),
        Some("storyboard") => run_storyboard(&args[1..]),
        Some("proxy") => run_proxy(&args[1..]),
        Some("proxy-dir") => run_proxy_dir(&args[1..]),
        Some("proxy-config") => run_proxy_config(&args[1..]),
        Some("extract-audio") => run_extract_audio(&args[1..]),
        Some("extract-audio-dir") => run_extract_audio_dir(&args[1..]),
        Some("project") => run_project(&args[1..]),
        Some("gitignore") => run_gitignore(&args[1..]),
        Some("organize") => run_organize(&args[1..]),
        Some("sequence") => run_sequence(&args[1..]),
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
