use std::path::Path;

use serde::Serialize;

use super::runner::{git_output, git_text, project_root};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RepositorySummaryDto {
    pub is_repo: bool,
    pub branch: Option<String>,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub operation: Option<String>,
}

fn parse_ahead_behind(value: &str) -> Result<(u32, u32), String> {
    let mut fields = value.split_whitespace();
    let ahead = fields
        .next()
        .ok_or("missing ahead count")?
        .parse::<u32>()
        .map_err(|e| e.to_string())?;
    let behind = fields
        .next()
        .ok_or("missing behind count")?
        .parse::<u32>()
        .map_err(|e| e.to_string())?;
    Ok((ahead, behind))
}

fn current_operation(project: &Path) -> Option<String> {
    let git_dir = git_text(project, &["rev-parse", "--git-dir"]).ok()?;
    let git_dir = Path::new(git_dir.trim());
    let git_dir = if git_dir.is_absolute() {
        git_dir.to_path_buf()
    } else {
        project.join(git_dir)
    };
    if git_dir.join("rebase-merge").exists() || git_dir.join("rebase-apply").exists() {
        Some("rebase".into())
    } else if git_dir.join("MERGE_HEAD").exists() {
        Some("merge".into())
    } else if git_dir.join("CHERRY_PICK_HEAD").exists() {
        Some("cherry_pick".into())
    } else if git_dir.join("REVERT_HEAD").exists() {
        Some("revert".into())
    } else {
        None
    }
}

fn operation_action_args(operation: &str, action: &str) -> Result<Vec<&'static str>, String> {
    let args = match (operation, action) {
        ("rebase", "continue") => vec!["rebase", "--continue"],
        ("rebase", "skip") => vec!["rebase", "--skip"],
        ("rebase", "abort") => vec!["rebase", "--abort"],
        ("cherry_pick", "continue") => vec!["cherry-pick", "--continue"],
        ("cherry_pick", "skip") => vec!["cherry-pick", "--skip"],
        ("cherry_pick", "abort") => vec!["cherry-pick", "--abort"],
        ("revert", "continue") => vec!["revert", "--continue"],
        ("revert", "skip") => vec!["revert", "--skip"],
        ("revert", "abort") => vec!["revert", "--abort"],
        // A merge is completed by committing the resolved index, so it only
        // supports aborting from here.
        ("merge", "abort") => vec!["merge", "--abort"],
        _ => return Err("unsupported git operation action".into()),
    };
    Ok(args)
}

pub(super) fn operation_action(project: &str, action: &str) -> Result<(), String> {
    let root = project_root(project)?;
    let operation = current_operation(&root).ok_or("no git operation in progress")?;
    let args = operation_action_args(&operation, action)?;
    let output = git_output(&root, &args)?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GitDiffStatEntryDto {
    pub path: String,
    pub added: u32,
    pub deleted: u32,
    pub binary: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GitDiffStatDto {
    pub staged: Vec<GitDiffStatEntryDto>,
    pub unstaged: Vec<GitDiffStatEntryDto>,
}

/// Parses `git diff --numstat -z` output. Rename records (empty path followed
/// by the old and new names) are skipped rather than risking a wrong path.
fn parse_numstat_z(raw: &[u8]) -> Vec<GitDiffStatEntryDto> {
    raw.split(|byte| *byte == 0)
        .filter_map(|record| {
            let mut fields = record.splitn(3, |byte| *byte == b'\t');
            let added = fields.next()?;
            let deleted = fields.next()?;
            let path = fields.next()?;
            if path.is_empty() {
                return None;
            }
            let count = |value: &[u8]| {
                std::str::from_utf8(value)
                    .ok()
                    .and_then(|value| value.parse::<u32>().ok())
                    .unwrap_or(0)
            };
            Some(GitDiffStatEntryDto {
                path: String::from_utf8_lossy(path).into_owned(),
                added: count(added),
                deleted: count(deleted),
                binary: added == b"-" || deleted == b"-",
            })
        })
        .collect()
}

pub(super) fn diff_stat(project: &str) -> Result<GitDiffStatDto, String> {
    let root = project_root(project)?;
    let collect = |args: &[&str]| -> Result<Vec<GitDiffStatEntryDto>, String> {
        let output = git_output(&root, args)?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
        }
        Ok(parse_numstat_z(&output.stdout))
    };
    Ok(GitDiffStatDto {
        staged: collect(&["diff", "--numstat", "-z", "--cached", "--", "."])?,
        unstaged: collect(&["diff", "--numstat", "-z", "--", "."])?,
    })
}

pub(super) fn repository_summary(project: &str) -> Result<RepositorySummaryDto, String> {
    let root = project_root(project)?;
    if !git_output(&root, &["rev-parse", "--git-dir"])?
        .status
        .success()
    {
        return Ok(RepositorySummaryDto {
            is_repo: false,
            branch: None,
            upstream: None,
            ahead: 0,
            behind: 0,
            operation: None,
        });
    }
    let branch = git_text(&root, &["branch", "--show-current"])?;
    let branch = (!branch.trim().is_empty()).then(|| branch.trim().to_string());
    let upstream_output = git_output(
        &root,
        &[
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            "@{upstream}",
        ],
    )?;
    let upstream = upstream_output.status.success().then(|| {
        String::from_utf8_lossy(&upstream_output.stdout)
            .trim()
            .to_string()
    });
    let (ahead, behind) = if upstream.is_some() {
        parse_ahead_behind(&git_text(
            &root,
            &["rev-list", "--left-right", "--count", "HEAD...@{upstream}"],
        )?)?
    } else {
        (0, 0)
    };
    Ok(RepositorySummaryDto {
        is_repo: true,
        branch,
        upstream: upstream.filter(|value| !value.is_empty()),
        ahead,
        behind,
        operation: current_operation(&root),
    })
}

pub(super) fn init_repository(project: &str) -> Result<(), String> {
    let root = project_root(project)?;
    // `git init` inside an existing repository creates a nested one, so treat
    // an existing .git (directory for normal repos, file for worktrees) as a
    // no-op and let the caller refresh instead.
    if root.join(".git").exists() {
        return Ok(());
    }
    let output = git_output(&root, &["init"])?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_DIR_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn temp_project(label: &str) -> std::path::PathBuf {
        let sequence = TEST_DIR_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "relaycat-status-{label}-{}-{sequence}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn initializes_a_repository_and_tolerates_a_second_call() {
        let root = temp_project("init");
        init_repository(root.to_str().unwrap()).unwrap();
        assert!(root.join(".git").is_dir());
        // A second initialization must not fail when the repository exists.
        init_repository(root.to_str().unwrap()).unwrap();
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn parses_numstat_entries_and_flags_binary_files() {
        let stats = parse_numstat_z(b"12\t3\tsrc/a.ts\0-\t-\tlogo.png\0");
        assert_eq!(stats.len(), 2);
        assert_eq!(stats[0].path, "src/a.ts");
        assert_eq!((stats[0].added, stats[0].deleted), (12, 3));
        assert!(!stats[0].binary);
        assert!(stats[1].binary);
    }

    #[test]
    fn parses_ahead_and_behind_counts() {
        assert_eq!(parse_ahead_behind("3\t2\n").unwrap(), (3, 2));
    }

    #[test]
    fn maps_operation_actions_to_git_arguments() {
        assert_eq!(
            operation_action_args("rebase", "continue").unwrap(),
            ["rebase", "--continue"]
        );
        assert_eq!(
            operation_action_args("merge", "abort").unwrap(),
            ["merge", "--abort"]
        );
        assert!(operation_action_args("merge", "continue").is_err());
        assert!(operation_action_args("rebase", "reset").is_err());
    }
}
