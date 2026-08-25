use std::{
    collections::{BTreeMap, HashSet},
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::Serialize;
use tokio::{fs, process::Command};
use uuid::Uuid;

const OPENTEAMS_DIR: &str = ".openteams";

#[derive(Clone)]
struct WorkspaceChangeJournal {
    _watcher: Arc<Mutex<RecommendedWatcher>>,
    changed_paths: Arc<Mutex<HashSet<PathBuf>>>,
    overflowed: Arc<AtomicBool>,
    workspace_path: PathBuf,
}

impl std::fmt::Debug for WorkspaceChangeJournal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkspaceChangeJournal")
            .field("workspace_path", &self.workspace_path)
            .field("overflowed", &self.overflowed.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl WorkspaceChangeJournal {
    fn start(workspace_path: &Path) -> Option<Self> {
        let workspace_path = canonicalize_lossy(workspace_path);
        let changed_paths = Arc::new(Mutex::new(HashSet::new()));
        let overflowed = Arc::new(AtomicBool::new(false));
        let callback_paths = changed_paths.clone();
        let callback_overflowed = overflowed.clone();

        let mut watcher =
            match notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
                match result {
                    Ok(event) => {
                        if matches!(event.kind, EventKind::Access(_)) {
                            return;
                        }
                        let mut paths =
                            callback_paths.lock().unwrap_or_else(|err| err.into_inner());
                        paths.extend(event.paths);
                    }
                    Err(err) => {
                        callback_overflowed.store(true, Ordering::Relaxed);
                        tracing::warn!(error = %err, "workspace change journal reported an error");
                    }
                }
            }) {
                Ok(watcher) => watcher,
                Err(err) => {
                    tracing::warn!(
                        workspace_path = %workspace_path.display(),
                        error = %err,
                        "failed to create workspace change journal; using untracked scan fallback"
                    );
                    return None;
                }
            };

        if let Err(err) = watcher.watch(&workspace_path, RecursiveMode::Recursive) {
            tracing::warn!(
                workspace_path = %workspace_path.display(),
                error = %err,
                "failed to watch workspace changes; using untracked scan fallback"
            );
            return None;
        }

        Some(Self {
            _watcher: Arc::new(Mutex::new(watcher)),
            changed_paths,
            overflowed,
            workspace_path,
        })
    }

    fn snapshot(&self) -> Result<Vec<String>, ()> {
        if self.overflowed.load(Ordering::Relaxed) {
            return Err(());
        }

        let paths = self
            .changed_paths
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let mut relative_paths = paths
            .iter()
            .filter_map(|path| journal_relative_path(&self.workspace_path, path))
            .collect::<Vec<_>>();
        relative_paths.sort();
        relative_paths.dedup();
        Ok(relative_paths)
    }
}

#[derive(Debug, Clone, Default)]
pub struct WorkspaceChangeBaseline {
    pub git_tree: Option<String>,
    pub untracked_files: Vec<String>,
    journal: Option<WorkspaceChangeJournal>,
}

#[derive(Debug, Clone, Default)]
pub struct WorkspaceChangeDelta {
    pub diff_patch: Option<String>,
    pub diff_paths: Vec<String>,
    pub untracked_files: Vec<String>,
    pub untracked_capture_incomplete: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceObservedPathRecord {
    pub path: String,
    pub source: String,
    pub existed_after_run: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified_at: Option<String>,
}

pub fn workspace_run_records_dir(workspace_path: &Path, session_id: Uuid) -> PathBuf {
    workspace_path
        .join(OPENTEAMS_DIR)
        .join("runs")
        .join(session_id.to_string())
        .join("run_records")
}

pub fn run_records_prefix(session_agent_id: Uuid, run_index: i64) -> String {
    format!("session_agent_{session_agent_id}_run_{run_index:04}")
}

pub async fn capture_workspace_change_baseline(workspace_path: &Path) -> WorkspaceChangeBaseline {
    let journal = WorkspaceChangeJournal::start(workspace_path);
    let git_tree = capture_baseline_git_tree(workspace_path).await;
    let untracked_files = if journal.is_some() {
        Vec::new()
    } else {
        capture_untracked_file_snapshot(workspace_path).await
    };
    tracing::debug!(
        workspace_path = %workspace_path.display(),
        baseline_has_git_tree = git_tree.is_some(),
        baseline_untracked_count = untracked_files.len(),
        baseline_uses_change_journal = journal.is_some(),
        "[workspace_change_capture] Captured workspace change baseline"
    );

    WorkspaceChangeBaseline {
        git_tree,
        untracked_files,
        journal,
    }
}

impl WorkspaceChangeBaseline {
    pub fn uses_change_journal(&self) -> bool {
        self.journal.is_some()
    }
}

pub async fn capture_workspace_change_delta(
    workspace_path: &Path,
    run_dir: &Path,
    session_agent_id: Uuid,
    run_index: i64,
    baseline: &WorkspaceChangeBaseline,
) -> WorkspaceChangeDelta {
    let (diff_patch, diff_paths) = match baseline.git_tree.as_deref() {
        Some(tree) => capture_git_diff_from_tree(workspace_path, tree)
            .await
            .map(|patch| filter_git_diff_to_observed_paths(&patch, workspace_path))
            .unwrap_or_default(),
        None => (String::new(), Vec::new()),
    };

    let mut diff_patch_written = false;
    if !diff_patch.trim().is_empty() && !diff_paths.is_empty() {
        let diff_path = run_dir.join(format!(
            "{}_diff.patch",
            run_records_prefix(session_agent_id, run_index)
        ));
        match fs::write(&diff_path, &diff_patch).await {
            Ok(_) => {
                diff_patch_written = true;
                tracing::debug!(
                    path = %diff_path.display(),
                    diff_patch_bytes = diff_patch.len(),
                    diff_path_count = diff_paths.len(),
                    "[workspace_change_capture] Wrote run-scoped diff patch"
                );
            }
            Err(err) => {
                tracing::warn!(
                    path = %diff_path.display(),
                    error = %err,
                    "failed to write run-scoped diff patch"
                );
            }
        }
    }

    let (untracked_files, untracked_capture_incomplete) =
        if let Some(journal) = baseline.journal.as_ref() {
            // Give native watcher callbacks a short opportunity to drain events queued immediately
            // before the executor exited. This is completion-path latency, not baseline latency.
            tokio::time::sleep(Duration::from_millis(100)).await;
            match journal.snapshot() {
                Ok(changed_paths) => (
                    capture_untracked_files_for_paths(workspace_path, &changed_paths).await,
                    false,
                ),
                Err(()) => {
                    tracing::warn!(
                        workspace_path = %workspace_path.display(),
                        "workspace change journal overflowed; untracked capture is incomplete"
                    );
                    (Vec::new(), true)
                }
            }
        } else {
            let baseline_untracked = baseline.untracked_files.iter().collect::<HashSet<_>>();
            (
                capture_untracked_file_snapshot(workspace_path)
                    .await
                    .into_iter()
                    .filter(|path| !baseline_untracked.contains(path))
                    .collect::<Vec<_>>(),
                false,
            )
        };

    tracing::debug!(
        workspace_path = %workspace_path.display(),
        run_dir = %run_dir.display(),
        session_agent_id = %session_agent_id,
        run_index,
        baseline_has_git_tree = baseline.git_tree.is_some(),
        diff_path_count = diff_paths.len(),
        diff_patch_bytes = diff_patch.len(),
        diff_patch_written,
        untracked_count = untracked_files.len(),
        untracked_capture_incomplete,
        "[workspace_change_capture] Captured workspace change delta"
    );

    WorkspaceChangeDelta {
        diff_patch: (!diff_patch.trim().is_empty() && !diff_paths.is_empty()).then_some(diff_patch),
        diff_paths,
        untracked_files,
        untracked_capture_incomplete,
    }
}

pub fn build_git_observed_path_records(
    workspace_path: &Path,
    diff_paths: &[String],
    untracked_files: &[String],
) -> Vec<WorkspaceObservedPathRecord> {
    let mut observed = BTreeMap::<String, WorkspaceObservedPathRecord>::new();

    for path in diff_paths {
        upsert_observed_path(&mut observed, workspace_path, path, "git_diff");
    }
    for path in untracked_files {
        upsert_observed_path(&mut observed, workspace_path, path, "git_untracked");
    }

    observed.into_values().collect()
}

fn upsert_observed_path(
    observed: &mut BTreeMap<String, WorkspaceObservedPathRecord>,
    workspace_path: &Path,
    relative_path: &str,
    source: &str,
) {
    let (existed_after_run, modified_at) = observed_file_metadata(workspace_path, relative_path);
    observed
        .entry(relative_path.to_string())
        .and_modify(|entry| {
            if !entry.source.split(',').any(|part| part.trim() == source) {
                entry.source.push(',');
                entry.source.push_str(source);
            }
            entry.existed_after_run |= existed_after_run;
            if entry.modified_at.is_none() {
                entry.modified_at = modified_at.clone();
            }
        })
        .or_insert_with(|| WorkspaceObservedPathRecord {
            path: relative_path.to_string(),
            source: source.to_string(),
            existed_after_run,
            modified_at,
        });
}

fn observed_file_metadata(workspace_path: &Path, relative_path: &str) -> (bool, Option<String>) {
    let absolute_path = workspace_path.join(relative_path);
    match std::fs::metadata(&absolute_path) {
        Ok(metadata) => {
            let modified_at = metadata
                .modified()
                .ok()
                .map(DateTime::<Utc>::from)
                .map(|dt| dt.to_rfc3339());
            (metadata.is_file(), modified_at)
        }
        Err(_) => (false, None),
    }
}

async fn capture_baseline_git_tree(workspace_path: &Path) -> Option<String> {
    if !is_git_worktree(workspace_path).await {
        return None;
    }

    let index_path = std::env::temp_dir().join(format!(
        "openteams-workspace-baseline-{}.index",
        Uuid::new_v4()
    ));

    let result = async {
        seed_baseline_index(workspace_path, &index_path).await?;
        if !run_git(workspace_path, &["add", "-u", "--", "."], Some(&index_path))
            .await
            .unwrap_or(false)
        {
            return None;
        }
        git_stdout(workspace_path, &["write-tree"], Some(&index_path)).await
    }
    .await;

    let _ = fs::remove_file(&index_path).await;
    let _ = fs::remove_file(index_path.with_extension("index.lock")).await;
    result
        .map(|tree| tree.trim().to_string())
        .filter(|tree| !tree.is_empty())
}

async fn seed_baseline_index(workspace_path: &Path, index_path: &Path) -> Option<()> {
    let git_dir = git_stdout(workspace_path, &["rev-parse", "--absolute-git-dir"], None)
        .await
        .map(|path| path.trim().to_string())
        .filter(|path| !path.is_empty());

    if let Some(git_dir) = git_dir {
        let source_index = PathBuf::from(git_dir).join("index");
        let started_at = Instant::now();
        match fs::copy(&source_index, index_path).await {
            Ok(_) => {
                tracing::debug!(
                    workspace_path = %workspace_path.display(),
                    source_index = %source_index.display(),
                    elapsed_ms = started_at.elapsed().as_millis(),
                    "copied git index for workspace baseline"
                );
                return Some(());
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                tracing::warn!(
                    workspace_path = %workspace_path.display(),
                    source_index = %source_index.display(),
                    error = %err,
                    "failed to copy git index; falling back to read-tree"
                );
            }
        }
    }

    let head_tree = git_stdout(
        workspace_path,
        &["rev-parse", "--verify", "HEAD^{tree}"],
        None,
    )
    .await
    .map(|tree| tree.trim().to_string())
    .filter(|tree| !tree.is_empty());
    let args = head_tree
        .as_deref()
        .map(|tree| vec!["read-tree", tree])
        .unwrap_or_else(|| vec!["read-tree", "--empty"]);

    run_git(workspace_path, &args, Some(index_path))
        .await
        .filter(|succeeded| *succeeded)
        .map(|_| ())
}

async fn is_git_worktree(workspace_path: &Path) -> bool {
    run_git(
        workspace_path,
        &["rev-parse", "--is-inside-work-tree"],
        None,
    )
    .await
    .unwrap_or(false)
}

async fn capture_git_diff_from_tree(workspace_path: &Path, tree: &str) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace_path)
        .args([
            "-c",
            "core.quotePath=false",
            "diff",
            "--no-color",
            tree,
            "--",
            ".",
        ])
        .output()
        .await
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let diff = String::from_utf8_lossy(&output.stdout).to_string();
    (!diff.trim().is_empty()).then_some(diff)
}

pub async fn capture_untracked_file_snapshot(workspace_path: &Path) -> Vec<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace_path)
        .args([
            "-c",
            "core.quotePath=false",
            "ls-files",
            "--others",
            "--exclude-standard",
            "-z",
        ])
        .output()
        .await;

    let output = match output {
        Ok(output) if output.status.success() => output,
        _ => return Vec::new(),
    };

    let mut files = Vec::new();
    for raw in output.stdout.split(|b| *b == b'\0') {
        if raw.is_empty() {
            continue;
        }
        let rel = String::from_utf8_lossy(raw).to_string();
        if let Some(path) = normalize_git_relative_path(&rel) {
            files.push(path);
        }
    }

    files.sort();
    files.dedup();
    files
}

async fn capture_untracked_files_for_paths(
    workspace_path: &Path,
    changed_paths: &[String],
) -> Vec<String> {
    const PATHS_PER_GIT_CALL: usize = 64;

    let mut files = Vec::new();
    for chunk in changed_paths.chunks(PATHS_PER_GIT_CALL) {
        let literal_pathspecs = chunk
            .iter()
            .map(|path| format!(":(literal){path}"))
            .collect::<Vec<_>>();
        let output = Command::new("git")
            .arg("-C")
            .arg(workspace_path)
            .args([
                "-c",
                "core.quotePath=false",
                "ls-files",
                "--others",
                "--exclude-standard",
                "-z",
                "--",
            ])
            .args(&literal_pathspecs)
            .output()
            .await;

        let Ok(output) = output else {
            continue;
        };
        if !output.status.success() {
            continue;
        }

        files.extend(
            output
                .stdout
                .split(|byte| *byte == b'\0')
                .filter(|raw| !raw.is_empty())
                .filter_map(|raw| normalize_git_relative_path(&String::from_utf8_lossy(raw))),
        );
    }

    files.sort();
    files.dedup();
    files
}

async fn run_git(workspace_path: &Path, args: &[&str], index_path: Option<&Path>) -> Option<bool> {
    let started_at = Instant::now();
    let mut command = Command::new("git");
    command.arg("-C").arg(workspace_path).args(args);
    if let Some(index_path) = index_path {
        command.env("GIT_INDEX_FILE", index_path);
    }
    let output = command.output().await.ok()?;
    tracing::debug!(
        workspace_path = %workspace_path.display(),
        args = ?args,
        elapsed_ms = started_at.elapsed().as_millis(),
        exit_status = ?output.status.code(),
        "workspace capture git command completed"
    );
    Some(output.status.success())
}

async fn git_stdout(
    workspace_path: &Path,
    args: &[&str],
    index_path: Option<&Path>,
) -> Option<String> {
    let started_at = Instant::now();
    let mut command = Command::new("git");
    command.arg("-C").arg(workspace_path).args(args);
    if let Some(index_path) = index_path {
        command.env("GIT_INDEX_FILE", index_path);
    }
    let output = command.output().await.ok()?;
    tracing::debug!(
        workspace_path = %workspace_path.display(),
        args = ?args,
        elapsed_ms = started_at.elapsed().as_millis(),
        exit_status = ?output.status.code(),
        "workspace capture git command completed"
    );
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).to_string())
}

fn filter_git_diff_to_observed_paths(diff: &str, _workspace_path: &Path) -> (String, Vec<String>) {
    let mut filtered = String::new();
    let mut observed_paths = Vec::new();

    for (path, patch) in split_git_diff_by_path(diff) {
        if normalize_git_relative_path(&path).is_none() {
            continue;
        }
        filtered.push_str(&patch);
        if !filtered.ends_with('\n') {
            filtered.push('\n');
        }
        observed_paths.push(path);
    }

    (filtered, observed_paths)
}

fn split_git_diff_by_path(diff: &str) -> BTreeMap<String, String> {
    let mut patches = BTreeMap::<String, String>::new();
    let mut current_path: Option<String> = None;
    let mut current_patch = String::new();

    for line in diff.split_inclusive('\n') {
        if let Some(next_path) = diff_header_path(line) {
            if let Some(path) = current_path.take()
                && !current_patch.trim().is_empty()
            {
                patches.insert(path, std::mem::take(&mut current_patch));
            }
            current_path = Some(next_path);
        }

        if current_path.is_some() {
            current_patch.push_str(line);
        }
    }

    if let Some(path) = current_path
        && !current_patch.trim().is_empty()
    {
        patches.insert(path, current_patch);
    }

    patches
}

fn diff_header_path(line: &str) -> Option<String> {
    let rest = line.strip_prefix("diff --git a/")?;
    let (old_path, new_path) = rest.split_once(" b/")?;
    let preferred = if new_path.trim() == "/dev/null" {
        old_path
    } else {
        new_path
    };
    normalize_git_relative_path(preferred)
}

fn normalize_git_relative_path(raw: &str) -> Option<String> {
    let trimmed = raw
        .trim()
        .trim_matches(|ch: char| {
            matches!(
                ch,
                '`' | '"' | '\'' | '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';'
            )
        })
        .trim_end_matches(['.', ':', '!', '?']);

    if trimmed.is_empty() || trimmed.contains("://") {
        return None;
    }

    let path = PathBuf::from(trimmed);
    if path.is_absolute() {
        return None;
    }

    let mut normalized = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => normalized.push(part.to_string_lossy().to_string()),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }

    if normalized.is_empty() {
        return None;
    }

    let mut relative = PathBuf::new();
    for part in &normalized {
        relative.push(part);
    }
    if is_internal_openteams_runtime_path(&relative) {
        return None;
    }

    Some(normalized.join("/"))
}

fn canonicalize_lossy(path: &Path) -> PathBuf {
    dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn journal_relative_path(workspace_path: &Path, path: &Path) -> Option<String> {
    let canonical_path;
    let relative = if let Ok(relative) = path.strip_prefix(workspace_path) {
        relative
    } else {
        canonical_path = canonicalize_lossy(path);
        canonical_path.strip_prefix(workspace_path).ok()?
    };
    let normalized = normalize_git_relative_path(&relative.to_string_lossy())?;
    let mut components = Path::new(&normalized).components();
    if components.next().is_some_and(
        |component| matches!(component, Component::Normal(part) if part == OPENTEAMS_DIR),
    ) {
        return None;
    }
    Some(normalized)
}

fn is_internal_openteams_runtime_path(path: &Path) -> bool {
    let components = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().to_string()),
            Component::CurDir => None,
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => None,
        })
        .collect::<Vec<_>>();

    match components.as_slice() {
        [openteams, runs, ..] if openteams == OPENTEAMS_DIR && runs == "runs" => true,
        [openteams, context, _session_id, file]
            if openteams == OPENTEAMS_DIR
                && context == "context"
                && matches!(
                    file.as_str(),
                    "messages.jsonl"
                        | "messages_compacted.background.jsonl"
                        | "shared_blackboard.jsonl"
                        | "work_records.jsonl"
                ) =>
        {
            true
        }
        [openteams, context, _session_id, internal_dir, ..]
            if openteams == OPENTEAMS_DIR
                && context == "context"
                && matches!(internal_dir.as_str(), "attachments" | "references") =>
        {
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use git::GitService;

    use super::*;

    #[tokio::test]
    async fn delta_diff_is_between_run_baseline_and_after_state() {
        let tempdir = tempfile::tempdir().expect("create tempdir");
        let repo_path = tempdir.path().join("repo");
        let git = GitService::new();
        git.initialize_repo_with_main_branch(&repo_path)
            .expect("init repo");

        std::fs::write(repo_path.join("shared.txt"), "alpha\nbeta\ngamma\n")
            .expect("write baseline");
        git.commit(&repo_path, "baseline").expect("commit baseline");

        std::fs::write(repo_path.join("shared.txt"), "ALPHA\nbeta\ngamma\n")
            .expect("write pre-existing session change");
        let baseline = capture_workspace_change_baseline(&repo_path).await;

        std::fs::write(repo_path.join("shared.txt"), "ALPHA\nBETA\ngamma\n")
            .expect("write current run change");
        let run_dir = tempdir.path().join("run-record");
        tokio::fs::create_dir_all(&run_dir)
            .await
            .expect("create run dir");

        let session_agent_id = Uuid::new_v4();
        let delta =
            capture_workspace_change_delta(&repo_path, &run_dir, session_agent_id, 1, &baseline)
                .await;

        assert_eq!(delta.diff_paths, vec!["shared.txt".to_string()]);
        let patch = delta.diff_patch.expect("delta patch");
        assert!(patch.contains("-beta"));
        assert!(patch.contains("+BETA"));
        assert!(!patch.contains("-alpha"));
        assert!(!patch.contains("+ALPHA"));
    }

    #[tokio::test]
    async fn delta_untracked_files_excludes_run_baseline_untracked_files() {
        let tempdir = tempfile::tempdir().expect("create tempdir");
        let repo_path = tempdir.path().join("repo");
        let git = GitService::new();
        git.initialize_repo_with_main_branch(&repo_path)
            .expect("init repo");
        std::fs::write(repo_path.join("tracked.txt"), "tracked\n").expect("write tracked");
        git.commit(&repo_path, "baseline").expect("commit baseline");

        std::fs::write(repo_path.join("other-session.txt"), "other\n").expect("write other");
        let baseline = capture_workspace_change_baseline(&repo_path).await;
        assert!(baseline.uses_change_journal());
        assert!(baseline.untracked_files.is_empty());

        std::fs::write(repo_path.join("current-session.txt"), "current\n").expect("write current");
        let run_dir = tempdir.path().join("run-record");
        tokio::fs::create_dir_all(&run_dir)
            .await
            .expect("create run dir");

        let delta =
            capture_workspace_change_delta(&repo_path, &run_dir, Uuid::new_v4(), 1, &baseline)
                .await;

        assert_eq!(
            delta.untracked_files,
            vec!["current-session.txt".to_string()]
        );
        assert!(!delta.untracked_capture_incomplete);
    }

    #[tokio::test]
    async fn baseline_copies_real_index_without_mutating_it() {
        let tempdir = tempfile::tempdir().expect("create tempdir");
        let repo_path = tempdir.path().join("repo");
        let git = GitService::new();
        git.initialize_repo_with_main_branch(&repo_path)
            .expect("init repo");
        std::fs::write(repo_path.join("tracked.txt"), "committed\n").expect("write tracked");
        git.commit(&repo_path, "baseline").expect("commit baseline");

        std::fs::write(repo_path.join("tracked.txt"), "working tree\n")
            .expect("write unstaged change");
        std::fs::write(repo_path.join("staged.txt"), "staged\n").expect("write staged file");
        let add_status = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo_path)
            .args(["add", "--", "staged.txt"])
            .status()
            .expect("stage file");
        assert!(add_status.success());

        let git_dir = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo_path)
            .args(["rev-parse", "--absolute-git-dir"])
            .output()
            .expect("resolve git dir");
        assert!(git_dir.status.success());
        let index_path =
            PathBuf::from(String::from_utf8_lossy(&git_dir.stdout).trim()).join("index");
        let index_before = std::fs::read(&index_path).expect("read index before capture");

        let baseline = capture_workspace_change_baseline(&repo_path).await;
        let tree = baseline.git_tree.expect("capture baseline tree");
        let index_after = std::fs::read(&index_path).expect("read index after capture");
        assert_eq!(index_before, index_after);

        let tree_files = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo_path)
            .args(["ls-tree", "-r", "--name-only", &tree])
            .output()
            .expect("list baseline tree");
        assert!(tree_files.status.success());
        let tree_files = String::from_utf8_lossy(&tree_files.stdout);
        assert!(tree_files.lines().any(|path| path == "staged.txt"));

        let tracked_contents = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo_path)
            .args(["show", &format!("{tree}:tracked.txt")])
            .output()
            .expect("read tracked file from baseline tree");
        assert!(tracked_contents.status.success());
        assert_eq!(tracked_contents.stdout, b"working tree\n");
    }
}
