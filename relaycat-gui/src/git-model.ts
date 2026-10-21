export interface GitActionPolicy {
  stage: { requiresConfirm: boolean };
  unstage: { requiresConfirm: boolean };
  discard: { requiresConfirm: boolean };
}

export function changeActionPolicy(_change: { conflict: boolean }): GitActionPolicy {
  return {
    stage: { requiresConfirm: false },
    unstage: { requiresConfirm: false },
    discard: { requiresConfirm: true },
  };
}

export function canCommit(message: string, stagedCount: number, amend: boolean): boolean {
  // Amending without a new message intentionally reuses the previous one, so
  // the backend resolves it instead of asking for a message again.
  const hasMessage = message.trim().length > 0 || amend;
  return hasMessage && (stagedCount > 0 || amend);
}

export function gitOperationControls(running: boolean) {
  return {
    fetchDisabled: running,
    pullDisabled: running,
    pushDisabled: running,
    branchDisabled: running,
  };
}

export type GitOperationAction = "continue" | "skip" | "abort";

/**
 * Which recovery actions make sense for the operation Git is paused on.
 * A merge is finished by committing the resolved index, so it only offers an
 * abort; rebase/cherry-pick/revert can continue, skip or abort.
 */
export function gitOperationActions(operation: string | null): GitOperationAction[] {
  if (!operation) return [];
  if (operation === "merge") return ["abort"];
  if (operation === "rebase" || operation === "cherry_pick" || operation === "revert") {
    return ["continue", "skip", "abort"];
  }
  return ["abort"];
}

export function historyFilterArgs(query: string, author: string, reference: string) {
  const value = (input: string) => input.trim() || null;
  return {
    query: value(query),
    author: value(author),
    reference: value(reference),
  };
}

export function reconcileGitChangeKeys(existing: string[], next: string[]) {
  const existingSet = new Set(existing);
  const nextSet = new Set(next);
  return {
    retained: existing.filter((key) => nextSet.has(key)),
    added: next.filter((key) => !existingSet.has(key)),
    removed: existing.filter((key) => !nextSet.has(key)),
  };
}

export function gitMutationUsesInlineFeedback(command: string): boolean {
  return new Set(["git_stage_paths", "git_unstage_paths", "git_apply_patch"]).has(command);
}

export type GitErrorCode =
  | "auth_failed"
  | "non_fast_forward"
  | "no_upstream"
  | "dirty_worktree"
  | "conflict"
  | "hook_failed"
  | "nothing_to_commit"
  | "detached_head"
  | "lock";

// Git runs with LC_ALL=C (see the Rust runner), so stderr is stable English
// and these patterns are reliable regardless of the user's locale.
const GIT_ERROR_PATTERNS: ReadonlyArray<readonly [GitErrorCode, RegExp]> = [
  [
    "auth_failed",
    /authentication failed|could not read username|permission denied \(publickey\)|invalid username or password|terminal prompts disabled/i,
  ],
  [
    "non_fast_forward",
    /non-fast-forward|updates were rejected|fetch first|remote contains work/i,
  ],
  ["no_upstream", /has no upstream branch|no upstream configured/i],
  [
    "dirty_worktree",
    /would be overwritten|please commit your changes or stash|your local changes/i,
  ],
  [
    "conflict",
    /automatic merge failed|fix conflicts|unmerged files|needs merge|cannot .* because you have unmerged|conflict \(|merge conflict/i,
  ],
  ["hook_failed", /hook declined|pre-commit|commit-msg|pre-push/i],
  [
    "nothing_to_commit",
    /nothing to commit|no changes added to commit|nothing added to commit/i,
  ],
  ["detached_head", /not currently on a branch|detached head/i],
  ["lock", /index\.lock|unable to create .*\.lock|another git process/i],
];

export function classifyGitError(message: string): GitErrorCode | null {
  for (const [code, pattern] of GIT_ERROR_PATTERNS) {
    if (pattern.test(message)) return code;
  }
  return null;
}

export function gitSyncButtonLabels(
  ahead: number,
  behind: number,
  pullLabel = "Pull",
  pushLabel = "Push",
) {
  const count = (label: string, value: number) =>
    Number.isFinite(value) && value > 0 ? `${label}(${Math.floor(value)})` : label;
  return {
    pull: count(pullLabel, behind),
    push: count(pushLabel, ahead),
  };
}

export interface GitDiffHunk {
  header: string;
  patch: string;
  lines: string[];
}

export interface GitDiffFile {
  path: string;
  header: string;
  hunks: GitDiffHunk[];
}

export function parseUnifiedDiff(content: string): GitDiffFile[] {
  const lines = content.split("\n");
  const files: GitDiffFile[] = [];
  let current: GitDiffFile | null = null;
  let headerLines: string[] = [];
  let hunkLines: string[] = [];

  const finishHunk = () => {
    if (!current || hunkLines.length === 0) return;
    const patchLines = [...headerLines, ...hunkLines];
    current.hunks.push({
      header: hunkLines[0],
      patch: `${patchLines.join("\n")}\n`,
      lines: hunkLines.slice(1),
    });
    hunkLines = [];
  };

  for (const line of lines) {
    if (line.startsWith("diff --git ")) {
      finishHunk();
      const match = /^diff --git a\/(.+) b\/(.+)$/.exec(line);
      current = { path: match?.[2] ?? "", header: "", hunks: [] };
      files.push(current);
      headerLines = [line];
    } else if (!current) {
      continue;
    } else if (line.startsWith("@@ ")) {
      finishHunk();
      current.header = headerLines.join("\n");
      hunkLines = [line];
    } else if (hunkLines.length > 0) {
      hunkLines.push(line);
    } else {
      headerLines.push(line);
    }
  }
  finishHunk();
  return files;
}

export function buildSelectedLinesPatch(
  fileHeader: string,
  hunkHeader: string,
  lines: string[],
  selected: ReadonlySet<number>,
): string {
  const match = /^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@(.*)$/.exec(hunkHeader);
  if (!match || selected.size === 0) return "";
  let oldCount = 0;
  let newCount = 0;
  const output: string[] = [];
  let previousIncluded = false;
  lines.forEach((line, index) => {
    const marker = line[0];
    if (marker === " ") {
      oldCount += 1;
      newCount += 1;
      output.push(line);
      previousIncluded = true;
    } else if (marker === "-") {
      oldCount += 1;
      newCount += selected.has(index) ? 0 : 1;
      output.push(selected.has(index) ? line : ` ${line.slice(1)}`);
      previousIncluded = true;
    } else if (marker === "+") {
      if (selected.has(index)) {
        newCount += 1;
        output.push(line);
        previousIncluded = true;
      } else {
        previousIncluded = false;
      }
    } else if (line.startsWith("\\ No newline") && previousIncluded) {
      output.push(line);
    }
  });
  const header = `@@ -${match[1]},${oldCount} +${match[2]},${newCount} @@${match[3]}`;
  return `${fileHeader}\n${header}\n${output.join("\n")}\n`;
}
