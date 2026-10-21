import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

import { workspacePreviewStartsCollapsed } from "../src/workspace-model.ts";

const css = readFileSync(new URL("../src/styles.css", import.meta.url), "utf8");
const html = readFileSync(new URL("../index.html", import.meta.url), "utf8");
const workspacePanel = readFileSync(
  new URL("../src/workspace-panel.ts", import.meta.url),
  "utf8",
);

test("git changes starts with the preview collapsed", () => {
  assert.equal(workspacePreviewStartsCollapsed("git"), true);
  assert.equal(workspacePreviewStartsCollapsed("files"), false);
  assert.equal(workspacePreviewStartsCollapsed("history"), false);
});

test("the file tree can be searched by path", () => {
  assert.match(html, /id="workspace-file-search"/);
  assert.match(workspacePanel, /search_workspace_files/);
  assert.match(workspacePanel, /workspace_file_search_empty/);
});

test("an upward pull at the top of a list refreshes that view", () => {
  assert.match(workspacePanel, /attachPullToRefresh/);
  assert.match(workspacePanel, /PULL_REFRESH_THRESHOLD/);
  assert.match(workspacePanel, /for \(const list of \[fileTree, gitList, historyList\]\)/);
});

test("switching views restores the previous view instead of rebuilding it", () => {
  const setMode = workspacePanel.match(
    /function setMode\(next: WorkspaceMode\)[\s\S]*?\n  }/,
  )?.[0] ?? "";
  assert.match(setMode, /previewRestore\[mode\]/);
  assert.match(setMode, /loadedViews\.has\(mode\)/);
  assert.match(
    setMode,
    /previewCollapseFor\(collapsedByMode\[mode\], mode, currentPanelWidth\(\)\)/,
  );
  assert.doesNotMatch(setMode, /^\s+void refresh\(\);$/m);
});

test("preview offers line numbers, a wrap toggle and a jump to the file", () => {
  assert.match(html, /id="workspace-preview-wrap"/);
  assert.match(workspacePanel, /relaycat\.workspacePreviewWrap/);
  assert.match(workspacePanel, /previewCode\.classList\.add\("show-line-numbers"\)/);
  assert.match(workspacePanel, /previewCode\.classList\.remove\("show-line-numbers"\)/);
  assert.match(workspacePanel, /workspace_view_file/);
  assert.match(css, /\.ws-preview-code\.wrap\s*{[^}]*white-space:\s*pre-wrap;/s);
  assert.match(css, /\.ws-preview-code\.show-line-numbers \.ws-code-line::before\s*{[^}]*counter\(ws-line\)/s);
});

test("the panel floor is 320px and sync buttons degrade when narrow", () => {
  assert.match(css, /\.workspace-panel\s*{[^}]*min-width:\s*320px;/s);
  assert.match(workspacePanel, /panel\.classList\.toggle\("narrow"/);
  assert.match(css, /\.workspace-panel\.narrow #workspace-pull::before\s*{[^}]*content:\s*"↓"/s);
  assert.match(
    css,
    /\.workspace-panel\.narrow #workspace-fetch,[\s\S]*?#workspace-push\s*{[^}]*font-size:\s*0;/,
  );
});

test("git history scrolls long rows without stretching its filters", () => {
  assert.match(
    css,
    /#workspace-history-view:not\(\[hidden\]\)\s*{[^}]*overflow:\s*hidden;/s,
  );
  assert.match(css, /\.ws-history-list\s*{[^}]*overflow:\s*auto;/s);
  assert.match(css, /\.ws-history-filters\s*{[^}]*width:\s*100%;[^}]*min-width:\s*0;/s);
});

test("git history flex layout applies only while the history tab is visible", () => {
  assert.match(css, /#workspace-history-view:not\(\[hidden\]\)\s*{[^}]*display:\s*flex;/s);
  assert.doesNotMatch(css, /#workspace-history-view\s*{[^}]*display:\s*flex;/s);
});

test("commit details scroll in both directions without widening the history layout", () => {
  assert.match(css, /\.ws-main\s*{[^}]*min-width:\s*0;/s);
  assert.match(
    css,
    /\.ws-preview\s*{[^}]*min-width:\s*0;[^}]*overflow:\s*hidden;/s,
  );
  assert.match(
    css,
    /\.ws-preview-code\s*{[^}]*width:\s*100%;[^}]*max-width:\s*100%;[^}]*overflow-x:\s*auto;[^}]*overflow-y:\s*auto;/s,
  );
});

test("the Git changes toolbar omits low-frequency branch management buttons", () => {
  assert.doesNotMatch(html, /id="workspace-new-branch"/);
  assert.doesNotMatch(html, /id="workspace-rename-branch"/);
  assert.doesNotMatch(html, /id="workspace-delete-branch"/);
});

test("the commit composer stays visible above the change list", () => {
  assert.match(html, /id="workspace-commit-bar"/);
  assert.match(html, /id="workspace-commit-status"/);
  assert.doesNotMatch(html, /workspace-commit-launcher/);
  assert.doesNotMatch(workspacePanel, /workspaceCommitPresentation/);
  assert.match(
    css,
    /#workspace-git-view:not\(\[hidden\]\)\s*{[^}]*overflow:\s*hidden;/s,
  );
  assert.match(css, /\.ws-git-list\s*{[^}]*overflow:\s*auto;/s);
  assert.match(css, /\.ws-commit-bar\s*{[^}]*border-top:\s*1px solid/s);
  assert.match(workspacePanel, /function updateCommitBar\(/);
});

test("file rows reserve a compact metadata column", () => {
  assert.match(css, /\.ws-tree-meta\s*{[^}]*white-space:\s*nowrap;/s);
  assert.match(workspacePanel, /formatWorkspaceEntrySize\(entry\.size_bytes,\s*entry\.is_dir\)/);
  assert.match(workspacePanel, /formatWorkspaceModifiedTime\(entry\.modified_unix_seconds/);
});

test("file previews own their horizontal and vertical scrolling", () => {
  assert.match(
    css,
    /\.ws-preview-code\s*{[^}]*overflow-x:\s*auto;[^}]*overflow-y:\s*auto;[^}]*overscroll-behavior:\s*contain;/s,
  );
  assert.match(css, /\.ws-code-line\s*{[^}]*width:\s*max-content;[^}]*min-width:\s*100%;/s);
});

test("remote Git output stays inside a scrollable operation log", () => {
  assert.match(
    css,
    /\.ws-operation-log\s*{[^}]*max-height:[^;]+;[^}]*overflow:\s*auto;/s,
  );
  assert.match(css, /\.ws-operation-progress\s*{[^}]*overflow:\s*hidden;/s);
  assert.match(css, /\.ws-operation\.success\s+\.ws-operation-progress-bar/s);
});

test("manual workspace refresh shows loading progress and completion feedback", () => {
  assert.match(html, /id="workspace-refresh-progress"[^>]*aria-live="polite"/);
  assert.match(css, /\.workspace-panel\.refreshing\s+\.ws-refresh-progress/s);
  assert.match(css, /\.workspace-panel\.refreshed\s+\.ws-refresh-progress-bar/s);
  assert.match(
    css,
    /\.ws-refresh-progress-bar\s*{[^}]*background:\s*linear-gradient\([^;]+#ff3b30[^;]+#ffcc00[^;]+#00e676[^;]+#00c7ff[^;]+#a855f7[^;]+#ff2d92[^;]+\);/s,
  );
  assert.match(workspacePanel, /setRefreshVisual\("running"\)/);
  assert.match(workspacePanel, /setRefreshVisual\("complete"\)/);
  assert.match(workspacePanel, /setRefreshVisual\("error"\)/);
  assert.match(css, /\.workspace-panel\.refresh-error\s+\.ws-refresh-progress-bar/s);
});

test("preview height is resizable and survives collapse and expand", () => {
  assert.match(html, /id="workspace-preview-resizer"/);
  assert.match(css, /\.ws-preview-resizer\s*{[^}]*cursor:\s*row-resize;/s);
  assert.match(
    workspacePanel,
    /workspaceMain\.style\.setProperty\("--workspace-preview-height"/,
  );
  assert.match(workspacePanel, /previewResizer\.addEventListener\("pointermove"/);
  assert.doesNotMatch(
    workspacePanel,
    /setPreviewCollapsed[\s\S]{0,400}removeProperty\("--workspace-preview-height"/,
  );
});

test("workspace remembers separate file Git and Shell widths", () => {
  assert.match(workspacePanel, /relaycat\.workspacePanelWidth\.files/);
  assert.match(workspacePanel, /relaycat\.workspacePanelWidth\.git/);
  assert.match(workspacePanel, /relaycat\.workspacePanelWidth\.shell/);
  assert.match(workspacePanel, /storedWorkspacePanelWidth\([^,]+,\s*440\)/s);
  assert.match(
    workspacePanel,
    /widthMode === "shell"[\s\S]*?storedWorkspacePanelWidth\(stored,\s*440\)/,
  );
  assert.match(
    workspacePanel,
    /value === "shell"[\s\S]*?window\.innerWidth \* 0\.6/,
  );
});

test("commit helpers recall recent messages and warn on long subjects", () => {
  assert.match(html, /id="workspace-commit-recent"/);
  assert.match(html, /id="workspace-commit-hint"/);
  assert.match(workspacePanel, /relaycat\.workspaceCommitHistory/);
  assert.match(workspacePanel, /workspace_commit_subject_long/);
  assert.match(workspacePanel, /workspace_commit_amend_placeholder/);
  assert.match(workspacePanel, /ArrowUp/);
});

test("the file tree can jump to the changes of one path", () => {
  assert.match(html, /id="workspace-git-filter"/);
  assert.match(workspacePanel, /openFileMenu/);
  assert.match(workspacePanel, /gitPathFilter/);
  assert.match(workspacePanel, /workspace_show_file_changes/);
  assert.match(workspacePanel, /allSections\.staged/);
});

test("switching projects drops stale branch and ref options", () => {
  const setProject = workspacePanel.match(
    /setProject\(nextProject\)[\s\S]*?if \(!project\) setOpened\(false\);/,
  )?.[0] ?? "";
  assert.match(setProject, /branchSelect\.innerHTML = ""/);
  assert.match(setProject, /historyRef\.innerHTML/);
  const notRepo = workspacePanel.match(
    /if \(!status\.is_repo\)[\s\S]*?renderState\(gitList, t\("workspace_not_repo"\)\)/,
  )?.[0] ?? "";
  assert.match(notRepo, /branchSelect\.innerHTML = ""/);
});

test("change list groups files by directory and remembers collapsed folders", () => {
  assert.match(workspacePanel, /groupChangesByDirectory\(items\)/);
  assert.match(workspacePanel, /showDirectories = groups\.length > 1/);
  assert.match(workspacePanel, /collapsedGitDirs\.has\(`\$\{kind\}:\$\{group\.dir\}`\)/);
  assert.match(css, /\.ws-git-dir\.collapsed \.ws-git-dir-chevron\s*{[^}]*rotate\(-90deg\)/s);
});

test("history filters collapse behind a toggle and show removable chips", () => {
  assert.match(html, /id="workspace-history-filters"[^>]*hidden/);
  assert.match(html, /id="workspace-history-filter-toggle"[^>]*aria-expanded="false"/);
  assert.match(workspacePanel, /setHistoryFiltersOpen\(false\)/);
  assert.match(workspacePanel, /workspace_filter_chip_query/);
  assert.match(workspacePanel, /historyDateLabel\(commit\.date\)/);
  assert.match(css, /\.ws-history-filters\[hidden\]\s*{[^}]*display:\s*none;/s);
});

test("Shell mode owns the workspace body without showing file or Git chrome", () => {
  assert.match(css, /\.workspace-panel\.shell-mode \.embedded-shell-panel\s*{[^}]*display:\s*flex;/s);
  assert.match(css, /\.workspace-panel\.shell-mode \.ws-main\s*{[^}]*display:\s*none;/s);
  assert.match(css, /\.workspace-panel\.shell-mode \.ws-tabs/s);
  assert.match(css, /\.workspace-panel\.shell-mode \.ws-refresh-progress/s);
});
