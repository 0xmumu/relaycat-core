use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const MAX_GIT_OUTPUT_BYTES: usize = 2 * 1024 * 1024;
const DEFAULT_GIT_TIMEOUT: Duration = Duration::from_secs(120);
const GIT_NOT_INSTALLED_ERROR: &str = "git_not_installed";
// Grandchildren (for example background processes started by Git hooks) can
// inherit the piped streams and keep the write ends open after `git` itself
// has exited, which would block a plain reader join forever. Wait for each
// reader with a bounded grace period and fail loudly instead of hanging.
const GIT_OUTPUT_DRAIN_GRACE: Duration = Duration::from_secs(5);

pub fn project_root(project: &str) -> Result<PathBuf, String> {
    let root = Path::new(project)
        .canonicalize()
        .map_err(|e| e.to_string())?;
    if !root.is_dir() {
        return Err("project path is not a directory".into());
    }
    Ok(root)
}

pub fn git_output(project: &Path, args: &[&str]) -> Result<Output, String> {
    git_output_with_timeout(project, args, DEFAULT_GIT_TIMEOUT)
}

pub fn git_output_with_timeout(
    project: &Path,
    args: &[&str],
    timeout: Duration,
) -> Result<Output, String> {
    let mut command = Command::new("git")
        .args(args)
        .current_dir(project)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "Never")
        // Stable English diagnostics so the UI can classify failures. Content
        // (paths, messages, diffs) is unaffected; only git's own output text
        // is pinned.
        .env("LC_ALL", "C")
        // The GUI has no terminal to host an editor. Commands that would open
        // one (for example `rebase --continue`) keep their existing message.
        .env("GIT_EDITOR", "true")
        .env("GIT_SEQUENCE_EDITOR", "true")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().map_err(git_spawn_error)?;
    let stdout = child.stdout.take().ok_or("git stdout unavailable")?;
    let stderr = child.stderr.take().ok_or("git stderr unavailable")?;
    let (stdout_tx, stdout_rx) = mpsc::channel::<Result<(Vec<u8>, bool), String>>();
    let (stderr_tx, stderr_rx) = mpsc::channel::<Result<(Vec<u8>, bool), String>>();
    thread::spawn(move || {
        let _ = stdout_tx.send(read_bounded_stream(stdout));
    });
    thread::spawn(move || {
        let _ = stderr_tx.send(read_bounded_stream(stderr));
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            break status;
        }
        if Instant::now() >= deadline {
            kill_process_group(&mut child);
            let _ = child.wait();
            // Reader threads are detached on purpose: a grandchild that still
            // holds the pipe can keep them blocked, and the command must not
            // wait for that to surface the timeout to the frontend.
            return Err("git command timed out".into());
        }
        thread::sleep(Duration::from_millis(20));
    };
    let (stdout, stdout_truncated) = match receive_bounded(&stdout_rx, GIT_OUTPUT_DRAIN_GRACE) {
        Ok(result) => result?,
        Err(error) => {
            kill_process_group(&mut child);
            return Err(error);
        }
    };
    let (stderr, stderr_truncated) = match receive_bounded(&stderr_rx, GIT_OUTPUT_DRAIN_GRACE) {
        Ok(result) => result?,
        Err(error) => {
            kill_process_group(&mut child);
            return Err(error);
        }
    };
    if stdout_truncated || stderr_truncated {
        return Err("git output exceeded limit".into());
    }
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

fn receive_bounded(
    receiver: &mpsc::Receiver<Result<(Vec<u8>, bool), String>>,
    wait: Duration,
) -> Result<Result<(Vec<u8>, bool), String>, String> {
    receiver
        .recv_timeout(wait)
        .map_err(|_| "git output stream stalled".to_string())
}

#[cfg(unix)]
fn kill_process_group(child: &mut std::process::Child) {
    let pid = child.id();
    if pid > 0 {
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    let _ = child.kill();
}

#[cfg(not(unix))]
fn kill_process_group(child: &mut std::process::Child) {
    let _ = child.kill();
}

fn git_spawn_error(error: std::io::Error) -> String {
    if error.kind() == std::io::ErrorKind::NotFound {
        GIT_NOT_INSTALLED_ERROR.to_string()
    } else {
        error.to_string()
    }
}

fn read_bounded_stream(mut stream: impl Read) -> Result<(Vec<u8>, bool), String> {
    let mut output = Vec::new();
    let mut truncated = false;
    let mut buffer = [0_u8; 8192];
    loop {
        let count = stream.read(&mut buffer).map_err(|e| e.to_string())?;
        if count == 0 {
            break;
        }
        let remaining = MAX_GIT_OUTPUT_BYTES.saturating_sub(output.len());
        let kept = remaining.min(count);
        output.extend_from_slice(&buffer[..kept]);
        truncated |= kept < count;
    }
    Ok((output, truncated))
}

pub fn git_text(project: &Path, args: &[&str]) -> Result<String, String> {
    let output = git_output(project, args)?;
    if !output.status.success() {
        return Err(bounded_text(&output.stderr, MAX_GIT_OUTPUT_BYTES));
    }
    Ok(bounded_text(&output.stdout, MAX_GIT_OUTPUT_BYTES))
}

fn bounded_text(bytes: &[u8], max_bytes: usize) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut used = 0;
    text.chars()
        .take_while(|ch| {
            let next = used + ch.len_utf8();
            if next > max_bytes {
                return false;
            }
            used = next;
            true
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io;
    use std::process::Command;
    use std::sync::mpsc;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use super::*;

    #[test]
    fn missing_git_executable_has_a_stable_error_code() {
        assert_eq!(
            git_spawn_error(io::Error::from(io::ErrorKind::NotFound)),
            "git_not_installed"
        );
    }

    #[test]
    fn output_is_capped_without_splitting_utf8() {
        assert_eq!(bounded_text("ab中文cd".as_bytes(), 5), "ab中");
    }

    #[test]
    fn stalled_output_streams_fail_instead_of_hanging() {
        let (_sender, receiver) = mpsc::channel::<Result<(Vec<u8>, bool), String>>();

        let error = receive_bounded(&receiver, Duration::from_millis(20)).unwrap_err();

        assert_eq!(error, "git output stream stalled");
    }

    #[test]
    fn command_output_is_rejected_after_the_limit() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let repo = std::env::temp_dir().join(format!(
            "relaycat-git-output-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&repo).unwrap();
        Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&repo)
            .status()
            .unwrap();
        let large = repo.join("large.bin");
        fs::write(&large, vec![b'x'; MAX_GIT_OUTPUT_BYTES + 1]).unwrap();
        let hash = Command::new("git")
            .args(["hash-object", "-w", "large.bin"])
            .current_dir(&repo)
            .output()
            .unwrap();
        let hash = String::from_utf8_lossy(&hash.stdout).trim().to_string();

        let result = git_output(&repo, &["cat-file", "blob", &hash]);
        fs::remove_dir_all(&repo).unwrap();
        assert!(result.unwrap_err().contains("output exceeded"));
    }

    #[cfg(unix)]
    fn temp_git_repo(label: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let repo = std::env::temp_dir().join(format!(
            "relaycat-git-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&repo).unwrap();
        let status = Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&repo)
            .status()
            .unwrap();
        assert!(status.success());
        repo
    }

    #[cfg(unix)]
    #[test]
    fn a_lingering_child_process_cannot_block_the_git_timeout() {
        let repo = temp_git_repo("timeout");
        let started = Instant::now();
        let result = git_output_with_timeout(
            &repo,
            &["-c", "alias.linger=!sleep 30", "linger"],
            Duration::from_millis(300),
        );
        let elapsed = started.elapsed();
        fs::remove_dir_all(&repo).unwrap();
        assert!(result.unwrap_err().contains("timed out"));
        assert!(
            elapsed < Duration::from_secs(10),
            "the process group should be killed promptly, took {elapsed:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_lingering_grandchild_is_killed_when_git_output_stalls() {
        let repo = temp_git_repo("stalled");
        let started = Instant::now();
        let result = git_output_with_timeout(
            &repo,
            &["-c", "alias.linger=!sleep 30 &", "linger"],
            Duration::from_secs(10),
        );
        let elapsed = started.elapsed();
        fs::remove_dir_all(&repo).unwrap();
        assert_eq!(result.unwrap_err(), "git output stream stalled");
        assert!(
            elapsed < Duration::from_secs(10),
            "the process group should be killed after the drain grace period, took {elapsed:?}"
        );
    }
}
