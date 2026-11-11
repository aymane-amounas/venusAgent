use anyhow::{Context, Result, bail};
use ignore::WalkBuilder;
use ring::digest::{SHA256, digest};
use std::collections::BTreeMap;
use std::process::Command;
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

const SKIP_DIRECTORIES: &[&str] = &[
    ".git",
    ".venus-agent",
    ".forge-agent",
    "node_modules",
    "target",
    "vendor",
    ".venv",
    "venv",
    "dist",
    "build",
];
const PROJECT_MANIFESTS: &[&str] = &[
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "go.mod",
    "Makefile",
];
const MAX_INSPECTED_FILES: usize = 100_000;
const MAX_WORKSPACE_FILE_BYTES: u64 = 256 * 1024;
const MAX_SEARCH_FILES: usize = 100_000;
const MAX_SEARCH_BYTES: u64 = 16 * 1024 * 1024;
const MAX_SEARCH_MATCHES: usize = 50;
const MAX_SEARCH_LINE_BYTES: usize = 512;
const MAX_REPLACEMENT_TEXT_BYTES: usize = 4096;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct WorkspaceSummary {
    pub file_count: usize,
    pub by_extension: BTreeMap<String, usize>,
    pub manifests: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct WorkspaceFile {
    pub path: String,
    pub content: String,
    pub size_bytes: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub struct WorkspaceReplacement {
    pub expected_sha256: String,
    pub new_content: String,
}

#[derive(Debug, PartialEq, Eq)]
pub struct WorkspaceSearchMatch {
    pub path: String,
    pub line_number: usize,
    pub line: String,
}

#[derive(Debug, PartialEq, Eq)]
pub struct WorkspaceSearch {
    pub files_scanned: usize,
    pub matches: Vec<WorkspaceSearchMatch>,
    pub truncated: bool,
}

type RepositorySummary = WorkspaceSummary;

pub fn workspace_summary(root: &Path) -> Result<WorkspaceSummary> {
    scan_workspace(root)
}

pub fn normalize_workspace(path: &Path) -> Result<PathBuf> {
    let root = path
        .canonicalize()
        .with_context(|| format!("Could not resolve workspace {}", path.display()))?;
    if !root.is_dir() {
        bail!("Workspace is not a directory: {}", root.display());
    }
    Ok(root)
}

pub fn normalize_workspace_relative_path(requested_path: &str) -> Result<String> {
    let path = workspace_relative_path(requested_path)?;
    Ok(workspace_relative_display_path(&path))
}

pub fn read_workspace_file(root: &Path, requested_path: &str) -> Result<WorkspaceFile> {
    let root = normalize_workspace(root)?;
    let relative_path = workspace_relative_path(requested_path)?;
    let candidate = root.join(&relative_path);
    let mut current = root.clone();
    for component in relative_path.components() {
        current.push(component);
        let metadata = fs::symlink_metadata(&current)
            .with_context(|| format!("Could not inspect workspace path {requested_path:?}"))?;
        if metadata.file_type().is_symlink() {
            bail!("Workspace file paths must not traverse symbolic links");
        }
    }

    let canonical = candidate
        .canonicalize()
        .with_context(|| format!("Could not resolve workspace file {requested_path:?}"))?;
    if !canonical.starts_with(&root) {
        bail!("Workspace file path must stay within the selected workspace");
    }
    let metadata = fs::metadata(&canonical)
        .with_context(|| format!("Could not inspect workspace file {requested_path:?}"))?;
    if !metadata.is_file() {
        bail!("Workspace path is not a regular file: {requested_path:?}");
    }
    if metadata.len() > MAX_WORKSPACE_FILE_BYTES {
        bail!(
            "Workspace file exceeds the {} KiB read limit",
            MAX_WORKSPACE_FILE_BYTES / 1024
        );
    }
    if !is_visible_workspace_file(&root, &canonical)? {
        bail!("Workspace file is hidden, ignored, or excluded: {requested_path:?}");
    }

    let bytes = read_bounded_file(&canonical, MAX_WORKSPACE_FILE_BYTES)?;
    let size_bytes = bytes.len();
    let content = String::from_utf8(bytes)
        .with_context(|| format!("Workspace file is not valid UTF-8 text: {requested_path:?}"))?;
    Ok(WorkspaceFile {
        path: workspace_relative_display_path(&relative_path),
        content,
        size_bytes,
    })
}

pub fn search_workspace(root: &Path, query: &str, case_sensitive: bool) -> Result<WorkspaceSearch> {
    let root = normalize_workspace(root)?;
    if query.trim().is_empty() {
        bail!("Workspace search query must not be empty");
    }
    if query.len() > 256 {
        bail!("Workspace search query must not exceed 256 bytes");
    }
    let needle = if case_sensitive {
        query.to_owned()
    } else {
        query.to_lowercase()
    };
    let mut result = WorkspaceSearch {
        files_scanned: 0,
        matches: Vec::new(),
        truncated: false,
    };
    let mut bytes_scanned = 0_u64;
    'entries: for entry in workspace_walker(&root).build() {
        let entry =
            entry.with_context(|| format!("Could not search workspace {}", root.display()))?;
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        if result.files_scanned == MAX_SEARCH_FILES {
            result.truncated = true;
            break;
        }
        result.files_scanned += 1;
        let metadata = entry.metadata().with_context(|| {
            format!(
                "Could not inspect workspace file {}",
                entry.path().display()
            )
        })?;
        if metadata.len() > MAX_WORKSPACE_FILE_BYTES {
            result.truncated = true;
            continue;
        }
        if bytes_scanned.saturating_add(metadata.len()) > MAX_SEARCH_BYTES {
            result.truncated = true;
            break;
        }
        bytes_scanned += metadata.len();
        let bytes = read_bounded_file(entry.path(), MAX_WORKSPACE_FILE_BYTES)?;
        let Ok(content) = String::from_utf8(bytes) else {
            continue;
        };
        if content.contains('\0') {
            continue;
        }
        let relative_path = entry
            .path()
            .strip_prefix(&root)
            .context("Workspace walker returned a path outside the selected workspace")?
            .to_path_buf();
        let relative_path = workspace_relative_display_path(&relative_path);
        for (index, line) in content.lines().enumerate() {
            let matches_query = if case_sensitive {
                line.contains(&needle)
            } else {
                line.to_lowercase().contains(&needle)
            };
            if !matches_query {
                continue;
            }
            if result.matches.len() == MAX_SEARCH_MATCHES {
                result.truncated = true;
                break 'entries;
            }
            result.matches.push(WorkspaceSearchMatch {
                path: relative_path.clone(),
                line_number: index + 1,
                line: truncate_utf8(line, MAX_SEARCH_LINE_BYTES),
            });
        }
    }
    Ok(result)
}

pub fn prepare_workspace_replacement(
    root: &Path,
    requested_path: &str,
    old_text: &str,
    new_text: &str,
) -> Result<WorkspaceReplacement> {
    if old_text.is_empty()
        || old_text.len() > MAX_REPLACEMENT_TEXT_BYTES
        || new_text.len() > MAX_REPLACEMENT_TEXT_BYTES
        || old_text.contains('\n')
        || old_text.contains('\r')
        || new_text.contains('\n')
        || new_text.contains('\r')
        || old_text.chars().any(char::is_control)
        || new_text.chars().any(char::is_control)
    {
        bail!(
            "Replacement text must be single-line, contain no control characters, and not exceed 4 KiB"
        );
    }
    let file = read_workspace_file(root, requested_path)?;
    if overlapping_match_count(&file.content, old_text) != 1 {
        bail!("The old text must match exactly one location in the current file");
    }
    let new_content = file.content.replacen(old_text, new_text, 1);
    if new_content.len() as u64 > MAX_WORKSPACE_FILE_BYTES {
        bail!("Updated workspace file would exceed the 256 KiB edit limit");
    }
    Ok(WorkspaceReplacement {
        expected_sha256: content_sha256(file.content.as_bytes()),
        new_content,
    })
}

pub fn replace_workspace_file(
    root: &Path,
    requested_path: &str,
    expected_sha256: &str,
    old_text: &str,
    new_text: &str,
) -> Result<WorkspaceFile> {
    let root = normalize_workspace(root)?;
    let replacement = prepare_workspace_replacement(&root, requested_path, old_text, new_text)?;
    if replacement.expected_sha256 != expected_sha256 {
        bail!("Workspace file changed after the edit was proposed; create a fresh proposal");
    }

    let relative_path = workspace_relative_path(requested_path)?;
    let target = root.join(&relative_path);
    let metadata = fs::metadata(&target)
        .with_context(|| format!("Could not inspect workspace file {requested_path:?}"))?;
    let parent = target
        .parent()
        .context("Workspace file path has no parent directory")?;
    let filename = target
        .file_name()
        .context("Workspace file path has no file name")?
        .to_string_lossy();
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("System clock is before the Unix epoch")?
        .as_nanos();
    let temporary_path = parent.join(format!(
        ".{filename}.venus-agent-{}-{nonce}.tmp",
        std::process::id()
    ));

    let current = read_workspace_file(&root, requested_path)?;
    if content_sha256(current.content.as_bytes()) != expected_sha256 {
        bail!("Workspace file changed after the edit was proposed; create a fresh proposal");
    }
    let write_result = (|| {
        let mut temporary = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary_path)
            .with_context(|| format!("Could not create {}", temporary_path.display()))?;
        temporary
            .write_all(replacement.new_content.as_bytes())
            .with_context(|| format!("Could not write {}", temporary_path.display()))?;
        temporary
            .set_permissions(metadata.permissions())
            .with_context(|| format!("Could not preserve permissions for {requested_path:?}"))?;
        temporary
            .sync_all()
            .with_context(|| format!("Could not sync {}", temporary_path.display()))?;
        Ok::<_, anyhow::Error>(())
    })();
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temporary_path);
        return Err(error);
    }
    if let Err(error) = fs::rename(&temporary_path, &target) {
        let _ = fs::remove_file(&temporary_path);
        return Err(error).with_context(|| format!("Could not apply edit to {requested_path:?}"));
    }
    read_workspace_file(&root, requested_path)
}

pub fn content_sha256(content: &[u8]) -> String {
    digest(&SHA256, content)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn inspect(root: &Path) -> Result<()> {
    let summary = scan_workspace(root)?;
    println!("Workspace: {}", root.display());
    println!("Files: {}", summary.file_count);
    if summary.manifests.is_empty() {
        println!("Project manifests: none detected");
    } else {
        println!("Project manifests: {}", summary.manifests.join(", "));
    }
    if summary.by_extension.is_empty() {
        println!("File types: none detected");
    } else {
        let file_types = summary
            .by_extension
            .iter()
            .map(|(extension, count)| format!("{extension} ({count})"))
            .collect::<Vec<_>>()
            .join(", ");
        println!("File types: {file_types}");
    }
    match git_status(root)? {
        Some(status) => {
            println!("Git status:");
            if status.trim().is_empty() {
                println!("  clean");
            } else {
                for line in status.lines() {
                    println!("  {line}");
                }
            }
        }
        None => println!("Git status: unavailable (Git executable not found)"),
    }
    Ok(())
}

fn scan_workspace(root: &Path) -> Result<RepositorySummary> {
    let mut summary = RepositorySummary::default();
    for entry in workspace_walker(root).build() {
        let entry =
            entry.with_context(|| format!("Could not scan workspace {}", root.display()))?;
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        summary.file_count += 1;
        if summary.file_count > MAX_INSPECTED_FILES {
            bail!(
                "Workspace inspection stopped after reaching the {MAX_INSPECTED_FILES}-file safety limit"
            );
        }
        if entry.depth() == 1 {
            let name = entry.file_name().to_string_lossy();
            if PROJECT_MANIFESTS.contains(&name.as_ref()) {
                summary.manifests.push(name.into_owned());
            }
        }
        if let Some(extension) = entry.path().extension().and_then(|ext| ext.to_str()) {
            *summary
                .by_extension
                .entry(format!(".{}", extension.to_lowercase()))
                .or_default() += 1;
        }
    }
    summary.manifests.sort();
    Ok(summary)
}

fn workspace_walker(root: &Path) -> WalkBuilder {
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(true)
        .git_ignore(true)
        .git_exclude(true)
        .require_git(false)
        .filter_entry(|entry| {
            !entry.file_type().is_some_and(|kind| kind.is_dir())
                || !SKIP_DIRECTORIES.contains(&entry.file_name().to_string_lossy().as_ref())
        });
    builder
}

fn workspace_relative_path(requested_path: &str) -> Result<PathBuf> {
    let path = Path::new(requested_path);
    if requested_path.trim().is_empty()
        || requested_path.len() > 1024
        || requested_path.chars().any(char::is_control)
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!(
            "Workspace file path must be a non-empty relative path without '.' or '..' components"
        );
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        normalized.push(component.as_os_str());
    }
    Ok(normalized)
}

fn workspace_relative_display_path(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn overlapping_match_count(content: &str, needle: &str) -> usize {
    let mut offset = 0;
    let mut count = 0;
    while offset < content.len() {
        let Some(relative_start) = content[offset..].find(needle) else {
            break;
        };
        let start = offset + relative_start;
        count += 1;
        offset = start
            + content[start..]
                .chars()
                .next()
                .map(char::len_utf8)
                .unwrap_or(needle.len());
    }
    count
}

fn is_visible_workspace_file(root: &Path, target: &Path) -> Result<bool> {
    let mut files_seen = 0;
    for entry in workspace_walker(root).build() {
        let entry =
            entry.with_context(|| format!("Could not scan workspace {}", root.display()))?;
        if entry.file_type().is_some_and(|kind| kind.is_file()) {
            files_seen += 1;
            if files_seen > MAX_INSPECTED_FILES {
                bail!(
                    "Workspace file visibility check stopped after reaching the {MAX_INSPECTED_FILES}-file safety limit"
                );
            }
            if entry.path() == target {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn read_bounded_file(path: &Path, max_bytes: u64) -> Result<Vec<u8>> {
    let file = File::open(path).with_context(|| format!("Could not read {}", path.display()))?;
    let mut bytes = Vec::new();
    file.take(max_bytes + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("Could not read {}", path.display()))?;
    if bytes.len() as u64 > max_bytes {
        bail!("File exceeds the configured read limit");
    }
    Ok(bytes)
}

fn truncate_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let suffix = "...";
    let mut end = max_bytes.saturating_sub(suffix.len());
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{}", &value[..end], suffix)
}

fn git_status(root: &Path) -> Result<Option<String>> {
    let output = match Command::new("git")
        .args([
            "-c",
            "core.fsmonitor=false",
            "status",
            "--short",
            "--branch",
        ])
        .current_dir(root)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
    {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("Could not start Git status"),
    };
    if output.status.success() {
        return Ok(Some(
            String::from_utf8(output.stdout).context("Git returned non-UTF-8 status output")?,
        ));
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("not a git repository") {
        return Ok(Some("not a Git repository".to_owned()));
    }
    bail!("Git status failed: {}", stderr.trim());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn scan_counts_workspace_files_and_skips_generated_directories() -> Result<()> {
        let temp = tempfile::tempdir()?;
        fs::create_dir_all(temp.path().join("src"))?;
        fs::create_dir_all(temp.path().join("node_modules/pkg"))?;
        fs::write(temp.path().join("Cargo.toml"), "[package]\n")?;
        fs::write(temp.path().join("src/main.rs"), "fn main() {}\n")?;
        fs::write(temp.path().join("node_modules/pkg/index.js"), "ignored\n")?;

        let summary = scan_workspace(temp.path())?;
        assert_eq!(summary.file_count, 2);
        assert_eq!(summary.by_extension.get(".rs"), Some(&1));
        assert_eq!(summary.manifests, ["Cargo.toml"]);
        Ok(())
    }

    #[test]
    fn normalize_workspace_rejects_missing_paths() {
        assert!(normalize_workspace(Path::new("/path/that/does/not/exist")).is_err());
    }

    #[test]
    fn workspace_relative_display_paths_use_forward_slashes() {
        let path = Path::new("src").join("main.rs");
        assert_eq!(workspace_relative_display_path(&path), "src/main.rs");
    }

    #[cfg(windows)]
    #[test]
    fn windows_workspace_paths_normalize_and_reject_escape_paths() {
        assert_eq!(
            normalize_workspace_relative_path(r"src\main.rs").unwrap(),
            "src/main.rs"
        );
        assert!(workspace_relative_path(r"C:\outside.txt").is_err());
        assert!(workspace_relative_path(r"\\server\share\outside.txt").is_err());
        assert!(workspace_relative_path(r"src\..\outside.txt").is_err());
    }

    #[test]
    fn reads_only_visible_text_files_within_the_workspace() -> Result<()> {
        let temp = tempfile::tempdir()?;
        fs::create_dir_all(temp.path().join("src"))?;
        fs::write(temp.path().join("src/main.rs"), "fn main() {}\n")?;
        fs::write(temp.path().join(".env"), "SECRET=value\n")?;
        fs::write(temp.path().join(".gitignore"), "private.txt\n")?;
        fs::write(temp.path().join("private.txt"), "not available\n")?;
        let file = read_workspace_file(temp.path(), "src/main.rs")?;
        assert_eq!(file.path, "src/main.rs");
        assert_eq!(file.content, "fn main() {}\n");
        assert_eq!(file.size_bytes, 13);
        assert!(read_workspace_file(temp.path(), "../outside.txt").is_err());
        assert!(read_workspace_file(temp.path(), ".env").is_err());
        assert!(read_workspace_file(temp.path(), "private.txt").is_err());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn reading_a_symlink_cannot_escape_the_workspace() -> Result<()> {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir()?;
        let outside = tempfile::tempdir()?;
        fs::write(outside.path().join("secret.txt"), "private\n")?;
        symlink(
            outside.path().join("secret.txt"),
            workspace.path().join("link.txt"),
        )?;
        assert!(read_workspace_file(workspace.path(), "link.txt").is_err());
        Ok(())
    }

    #[test]
    fn search_honors_ignore_rules_and_caps_returned_matches() -> Result<()> {
        let temp = tempfile::tempdir()?;
        fs::create_dir_all(temp.path().join("src"))?;
        fs::write(temp.path().join(".gitignore"), "private.txt\n")?;
        fs::write(temp.path().join("private.txt"), "needle hidden\n")?;
        fs::write(
            temp.path().join("src/search.rs"),
            (0..60)
                .map(|line| format!("Needle on line {line}\n"))
                .collect::<String>(),
        )?;
        let result = search_workspace(temp.path(), "needle", false)?;
        assert_eq!(result.matches.len(), MAX_SEARCH_MATCHES);
        assert!(result.truncated);
        assert_eq!(result.matches[0].path, "src/search.rs");
        assert_eq!(result.matches[0].line_number, 1);
        assert_eq!(
            search_workspace(temp.path(), "needle", true)?.matches.len(),
            0
        );
        Ok(())
    }

    #[test]
    fn search_snippets_respect_byte_limits_without_splitting_utf8() {
        let line = format!("{}終", "a".repeat(MAX_SEARCH_LINE_BYTES));
        let snippet = truncate_utf8(&line, MAX_SEARCH_LINE_BYTES);
        assert_eq!(snippet.len(), MAX_SEARCH_LINE_BYTES);
        assert!(snippet.ends_with("..."));
        assert!(snippet.is_char_boundary(snippet.len()));
    }

    #[test]
    fn replacement_requires_one_exact_match_and_refuses_stale_content() -> Result<()> {
        let temp = tempfile::tempdir()?;
        fs::write(temp.path().join("src.txt"), "alpha = 1\nbeta = 2\n")?;
        let proposal =
            prepare_workspace_replacement(temp.path(), "src.txt", "alpha = 1", "alpha = 3")?;
        assert_eq!(proposal.new_content, "alpha = 3\nbeta = 2\n");
        fs::write(temp.path().join("src.txt"), "alpha = 1\nbeta = 4\n")?;
        assert!(
            replace_workspace_file(
                temp.path(),
                "src.txt",
                &proposal.expected_sha256,
                "alpha = 1",
                "alpha = 3"
            )
            .is_err()
        );
        assert_eq!(
            fs::read_to_string(temp.path().join("src.txt"))?,
            "alpha = 1\nbeta = 4\n"
        );
        Ok(())
    }

    #[test]
    fn replacement_changes_only_one_unique_visible_line() -> Result<()> {
        let temp = tempfile::tempdir()?;
        fs::write(temp.path().join("src.txt"), "alpha = 1\nbeta = 2\n")?;
        let proposal =
            prepare_workspace_replacement(temp.path(), "src.txt", "alpha = 1", "alpha = 3")?;
        let changed = replace_workspace_file(
            temp.path(),
            "src.txt",
            &proposal.expected_sha256,
            "alpha = 1",
            "alpha = 3",
        )?;
        assert_eq!(changed.content, "alpha = 3\nbeta = 2\n");
        assert_eq!(
            fs::read_to_string(temp.path().join("src.txt"))?,
            changed.content
        );
        Ok(())
    }

    #[test]
    fn replacement_rejects_ambiguous_or_multiline_changes() -> Result<()> {
        let temp = tempfile::tempdir()?;
        fs::write(temp.path().join("src.txt"), "repeat\nrepeat\n")?;
        assert!(
            prepare_workspace_replacement(temp.path(), "src.txt", "repeat", "changed").is_err()
        );
        assert!(
            prepare_workspace_replacement(temp.path(), "src.txt", "repeat\nrepeat", "changed")
                .is_err()
        );
        assert!(
            prepare_workspace_replacement(temp.path(), "src.txt", "repeat", "one\ntwo").is_err()
        );
        Ok(())
    }

    #[test]
    fn replacement_rejects_overlapping_matches_and_terminal_controls() -> Result<()> {
        let temp = tempfile::tempdir()?;
        fs::write(temp.path().join("src.txt"), "aaa\n")?;
        assert!(prepare_workspace_replacement(temp.path(), "src.txt", "aa", "b").is_err());
        assert!(
            prepare_workspace_replacement(temp.path(), "src.txt", "aaa", "\u{1b}[31mred").is_err()
        );
        Ok(())
    }
}
