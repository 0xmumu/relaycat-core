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

#[cfg(test)]
mod tests {
    use super::*;

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
