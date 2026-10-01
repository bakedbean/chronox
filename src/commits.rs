//! Reconstruct changes the agent committed but did not make through an edit
//! tool — `sed -i`, heredoc rewrites, scripts run from the shell. Those leave
//! no Edit/Write call in the session log, but the commits survive in git. We
//! find the commits made during a session, ask git for each one's patch, and
//! turn every hunk into a `ChangeEvent` shaped like an Edit (old = context +
//! removed, new = context + added).
//!
//! Every commit must also have been made within a session's time span: a sha
//! in a log only proves the commit exists (a user may paste an old one), not
//! that the session made it. Commits are found three ways, so any one is
//! enough:
//! - `[branch sha] subject` output of a `git commit` in a session log;
//! - the worktree's HEAD reflog entries (`commit:`, `commit (amend):`, …) whose
//!   commit time falls within a session's span — this catches `git commit -q`,
//!   which prints nothing;
//! - `<sha> <subject>` lines (`git log --oneline`) in the output of a tool call
//!   whose command ran `git commit`, again only within a session's span — this
//!   catches quiet commits whose reflog has since been lost.
//!
//! A committed file is skipped when an edit tool already recorded a change to
//! it since the previous commit, so tool-tracked edits are never shown twice.

use sessionx::extract::parse_iso8601_ms;
use sessionx::{ChangeDetail, ChangeEvent, ChangeSource, ChangeTool};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Max chars of `<short sha> <subject>` shown as an event's summary.
const SUMMARY_MAX: usize = 80;

/// Slack around a session's first/last log timestamp when matching reflog
/// commits: commit times have one-second resolution.
const SPAN_SLACK_MS: i64 = 2_000;

/// One file's change in a commit's patch.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FileChange {
    /// New file: its full content.
    Added(String),
    /// Removed file: its full former content.
    Deleted(String),
    /// Changed file: `(old, new)` per hunk, each side including context lines.
    Modified(Vec<(String, String)>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileDiff {
    /// Path relative to the repository root.
    path: String,
    change: FileChange,
}

/// A commit's metadata — cheap to fetch, enough to reject it before loading
/// its patch.
#[derive(Debug, Clone)]
struct CommitMeta {
    full: String,
    parent: String,
    ts_ms: i64,
    subject: String,
}

#[derive(Debug, Clone)]
struct Commit {
    full: String,
    parent: String,
    ts_ms: i64,
    subject: String,
    files: Vec<FileDiff>,
    /// Hash of the patch's added/removed lines (not context or line numbers),
    /// like `git patch-id`: equal across a rebase that applied cleanly.
    patch_id: u64,
}

/// Incremental scan state for one session log.
#[derive(Debug, Default)]
struct LogScan {
    /// Bytes consumed so far (always at a line boundary).
    offset: u64,
    /// Commit shas reported by `git commit` output, in order of appearance.
    shas: Vec<String>,
    /// Shas from `--oneline`-style lines in the output of a `git commit` tool
    /// call. Weaker evidence (it may list older commits too), so these are
    /// only used when the commit time falls within a session's span.
    oneline_shas: Vec<String>,
    /// Ids of tool calls whose input mentions `git commit`; their results are
    /// scanned for `oneline_shas`.
    commit_calls: HashSet<String>,
    /// Earliest and latest line `timestamp` seen, epoch ms.
    span: Option<(i64, i64)>,
}

/// Commit-derived events for a worktree, cached across refreshes. Session logs
/// are append-only, so each log is scanned incrementally from where the last
/// scan stopped, and nothing is recomputed until one grows. Commits are
/// immutable, so each sha's metadata is fetched once, and a patch only for
/// commits made within a session.
#[derive(Debug, Default)]
pub struct CommitIndex {
    scanned: HashMap<PathBuf, LogScan>,
    /// HEAD reflog `git commit` entries; `None` until first read.
    reflog: Option<Vec<ReflogCommit>>,
    /// Sha (abbreviated or full) -> metadata; `None` when it isn't a non-merge
    /// commit in this repository.
    meta: HashMap<String, Option<CommitMeta>>,
    /// Full sha -> commit with its patch.
    loaded: HashMap<String, Commit>,
    /// `git rev-parse --show-toplevel`, once it succeeds.
    toplevel: Option<PathBuf>,
    /// Full sha -> (HEAD it was checked against, reachable from that HEAD).
    reachable: HashMap<String, (String, bool)>,
    /// The events returned by the last refresh and the tool-event count they
    /// were built against, reused until a log grows.
    last: Option<(usize, Vec<ChangeEvent>)>,
}

#[derive(Debug, Clone)]
struct ReflogCommit {
    sha: String,
    ts_ms: i64,
    /// Made by `git commit --amend`, replacing a same-parent sibling.
    amend: bool,
}

impl CommitIndex {
    /// Scan `sessions` for commits and return events for the committed changes
    /// that `tool_events` (the edit-tool timeline) doesn't already cover.
    pub fn refresh(
        &mut self,
        worktree: &Path,
        sessions: &[PathBuf],
        tool_events: &[ChangeEvent],
    ) -> Vec<ChangeEvent> {
        let present: HashSet<&PathBuf> = sessions.iter().collect();
        self.scanned.retain(|p, _| present.contains(p));
        let mut grew = false;
        for path in sessions {
            let scan = self.scanned.entry(path.clone()).or_default();
            let before = scan.offset;
            scan_log(path, scan);
            grew |= scan.offset != before;
        }

        if !grew
            && let Some((n, last)) = &self.last
            && *n == tool_events.len()
        {
            return last.clone();
        }
        if self.toplevel.is_none() {
            self.toplevel = git_toplevel(worktree);
        }
        let Some(toplevel) = self.toplevel.clone() else {
            return Vec::new();
        };
        self.reflog = Some(reflog_commits(&toplevel));

        let spans: Vec<(i64, i64)> = sessions
            .iter()
            .filter_map(|p| self.scanned.get(p)?.span)
            .collect();
        let in_session = |ts: i64| {
            spans
                .iter()
                .any(|(lo, hi)| ts >= lo - SPAN_SLACK_MS && ts <= hi + SPAN_SLACK_MS)
        };
        // In evidence order: each log's shas in the order they appeared, then
        // the reflog (whose times are known, so out-of-span entries drop here).
        let candidates: Vec<String> = sessions
            .iter()
            .filter_map(|p| self.scanned.get(p))
            .flat_map(|s| s.shas.iter().chain(&s.oneline_shas).cloned())
            .chain(
                self.reflog
                    .iter()
                    .flatten()
                    .filter(|r| in_session(r.ts_ms))
                    .map(|r| r.sha.clone()),
            )
            .collect();

        let mut accepted: Vec<CommitMeta> = Vec::new();
        for sha in &candidates {
            let meta = self
                .meta
                .entry(sha.clone())
                .or_insert_with(|| commit_meta(&toplevel, sha));
            if let Some(m) = meta
                && in_session(m.ts_ms)
                && !accepted.iter().any(|a| a.full == m.full)
            {
                accepted.push(m.clone());
            }
        }
        for m in &accepted {
            if !self.loaded.contains_key(&m.full)
                && let Some(c) = load_commit(&toplevel, m.clone())
            {
                self.loaded.insert(m.full.clone(), c);
            }
        }
        let commits: Vec<&Commit> = accepted
            .iter()
            .filter_map(|m| self.loaded.get(&m.full))
            .collect();
        let head = git(&toplevel, &["rev-parse", "HEAD"]).unwrap_or_default();
        let head = head.trim();
        for c in &commits {
            let stale = self.reachable.get(&c.full).is_none_or(|(h, _)| h != head);
            if stale {
                let r = !head.is_empty()
                    && git(&toplevel, &["merge-base", "--is-ancestor", &c.full, head]).is_some();
                self.reachable.insert(c.full.clone(), (head.to_string(), r));
            }
        }
        let amends: HashSet<&str> = self
            .reflog
            .iter()
            .flatten()
            .filter(|r| r.amend)
            .map(|r| r.sha.as_str())
            .collect();
        let reachable = |c: &Commit| self.reachable.get(&c.full).is_some_and(|(_, r)| *r);
        let kept = drop_superseded(commits, &reachable, &amends);
        let events = build_events(&toplevel, worktree, kept, tool_events);
        self.last = Some((tool_events.len(), events.clone()));
        events
    }
}

/// Commits recorded in the worktree's HEAD reflog by `git commit` (including
/// amend/initial). Rebases, resets and checkouts are excluded: they move HEAD
/// without the agent writing new changes.
fn reflog_commits(dir: &Path) -> Vec<ReflogCommit> {
    let Some(out) = git(dir, &["log", "-g", "--format=%H%x00%ct%x00%gs", "HEAD"]) else {
        return Vec::new();
    };
    out.lines()
        .filter_map(|l| {
            let mut p = l.splitn(3, '\0');
            let (sha, ts, gs) = (p.next()?, p.next()?, p.next()?);
            if !gs.starts_with("commit") {
                return None;
            }
            Some(ReflogCommit {
                sha: sha.to_string(),
                ts_ms: ts.parse::<i64>().ok()? * 1000,
                amend: gs.starts_with("commit (amend)"),
            })
        })
        .collect()
}

fn patch_id(patch: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    patch
        .lines()
        .filter(|l| l.starts_with('+') || l.starts_with('-'))
        .for_each(|l| l.hash(&mut h));
    h.finish()
}

/// Drop commits that history has replaced, returning the rest with the time to
/// show them at, oldest-first. A commit is replaced only when HEAD no longer
/// reaches it *and* a reachable commit took its place:
/// - a rebased copy (same patch) — shown at the original's time, when the edit
///   was actually made;
/// - an amended version (same parent, and same subject or an amend per the
///   reflog).
///
/// Reachable commits are never dropped, so a change reapplied after a revert,
/// or two branches from one base, all stay.
fn drop_superseded<'a>(
    commits: Vec<&'a Commit>,
    reachable: &dyn Fn(&Commit) -> bool,
    amends: &HashSet<&str>,
) -> Vec<(&'a Commit, i64)> {
    let live: Vec<&Commit> = commits.iter().copied().filter(|c| reachable(c)).collect();
    let mut shown: HashMap<&str, i64> = live.iter().map(|c| (c.full.as_str(), c.ts_ms)).collect();
    let mut kept: Vec<&Commit> = live.clone();
    for c in commits.iter().copied().filter(|c| !reachable(c)) {
        if let Some(copy) = live.iter().find(|l| l.patch_id == c.patch_id) {
            let t = shown.get_mut(copy.full.as_str()).expect("live commit");
            *t = (*t).min(c.ts_ms);
            continue;
        }
        let amended = live.iter().any(|l| {
            l.parent == c.parent && (l.subject == c.subject || amends.contains(l.full.as_str()))
        });
        if !amended {
            kept.push(c);
            shown.insert(c.full.as_str(), c.ts_ms);
        }
    }
    let mut out: Vec<(&Commit, i64)> = kept
        .into_iter()
        .map(|c| (c, shown[c.full.as_str()]))
        .collect();
    out.sort_by_key(|(_, t)| *t);
    out
}

/// Turn `commits` (oldest-first) into events, skipping a commit's file when an
/// edit tool changed that file after the previous commit and up to this one.
fn build_events(
    toplevel: &Path,
    worktree: &Path,
    commits: Vec<(&Commit, i64)>,
    tool_events: &[ChangeEvent],
) -> Vec<ChangeEvent> {
    let tool_paths: Vec<(PathBuf, i64)> = tool_events
        .iter()
        .map(|e| (absolutize(worktree, &e.file_path), e.timestamp_ms))
        .collect();
    let mut out = Vec::new();
    let mut prev_ts = i64::MIN;
    for (c, ts_ms) in commits {
        let short = &c.full[..c.full.len().min(7)];
        let summary = clip(&format!("{short} {}", c.subject), SUMMARY_MAX);
        let source_file = PathBuf::from(format!("git:{}", c.full));
        for (fi, f) in c.files.iter().enumerate() {
            let path = toplevel.join(&f.path);
            let covered = tool_paths
                .iter()
                .any(|(p, ts)| *p == path && *ts > prev_ts && *ts <= ts_ms);
            if covered {
                continue;
            }
            let mk = |hi: usize, tool: ChangeTool, detail: ChangeDetail| ChangeEvent {
                timestamp_ms: ts_ms,
                tool,
                file_path: path.clone(),
                summary: summary.clone(),
                detail,
                source: ChangeSource {
                    session_file: source_file.clone(),
                    line_index: fi,
                    index_in_line: hi,
                },
            };
            match &f.change {
                FileChange::Added(content) => out.push(mk(
                    0,
                    ChangeTool::Write,
                    ChangeDetail::Write {
                        head: content.clone(),
                    },
                )),
                FileChange::Deleted(content) => out.push(mk(
                    0,
                    ChangeTool::Edit,
                    ChangeDetail::Edit {
                        old: content.clone(),
                        new: String::new(),
                    },
                )),
                FileChange::Modified(hunks) => {
                    for (hi, (old, new)) in hunks.iter().enumerate() {
                        out.push(mk(
                            hi,
                            ChangeTool::Edit,
                            ChangeDetail::Edit {
                                old: old.clone(),
                                new: new.clone(),
                            },
                        ));
                    }
                }
            }
        }
        prev_ts = ts_ms;
    }
    out
}

/// Agents usually report absolute paths, but a relative one is relative to the
/// worktree.
fn absolutize(worktree: &Path, p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::fs::canonicalize(worktree)
            .unwrap_or_else(|_| worktree.to_path_buf())
            .join(p)
    }
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}

/// Read the complete lines appended to `path` since `scan.offset`, collecting
/// commit shas and the timestamp span, and advance the offset past them. A log
/// that shrank (rewritten) is rescanned from the start.
fn scan_log(path: &Path, scan: &mut LogScan) {
    let Ok(mut file) = std::fs::File::open(path) else {
        return;
    };
    let Ok(len) = file.metadata().map(|m| m.len()) else {
        return;
    };
    if len < scan.offset {
        *scan = LogScan::default();
    }
    if len == scan.offset || file.seek(SeekFrom::Start(scan.offset)).is_err() {
        return;
    }
    let mut buf = Vec::new();
    if file.read_to_end(&mut buf).is_err() {
        return;
    }
    // Leave a partially written last line for the next scan.
    let Some(end) = buf.iter().rposition(|&b| b == b'\n') else {
        return;
    };
    for line in String::from_utf8_lossy(&buf[..end]).lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if let Some(ts) = v
            .get("timestamp")
            .and_then(|t| t.as_str())
            .and_then(parse_iso8601_ms)
        {
            let (lo, hi) = scan.span.get_or_insert((ts, ts));
            *lo = (*lo).min(ts);
            *hi = (*hi).max(ts);
        }
        // Commit output always contains "] "; skip walking every other line.
        if line.contains("] ") {
            visit_strings(&v, &mut |s| {
                for sha in commit_shas_in(s) {
                    if !scan.shas.contains(&sha) {
                        scan.shas.push(sha);
                    }
                }
            });
        }
        if line.contains("git commit") {
            record_commit_calls(&v, &mut scan.commit_calls);
        }
        if !scan.commit_calls.is_empty() {
            visit_commit_results(&v, &scan.commit_calls, &mut |s| {
                for sha in oneline_shas_in(s) {
                    if !scan.oneline_shas.contains(&sha) {
                        scan.oneline_shas.push(sha);
                    }
                }
            });
        }
    }
    scan.offset += end as u64 + 1;
}

/// Call `f` on every string anywhere in `v`. Harness-agnostic: Claude, Codex
/// and pi nest tool output differently, but it's always a string somewhere.
fn visit_strings(v: &serde_json::Value, f: &mut impl FnMut(&str)) {
    match v {
        serde_json::Value::String(s) => f(s),
        serde_json::Value::Array(a) => a.iter().for_each(|x| visit_strings(x, f)),
        serde_json::Value::Object(o) => o.values().for_each(|x| visit_strings(x, f)),
        _ => {}
    }
}

/// Keys naming a tool call on the call itself (Claude `tool_use.id`, pi
/// `toolCall.id`, Codex `function_call.call_id`) and on its result (Claude
/// `tool_use_id`, pi `toolCallId`, Codex `function_call_output.call_id`).
const CALL_ID_KEYS: [&str; 2] = ["id", "call_id"];
const RESULT_ID_KEYS: [&str; 3] = ["tool_use_id", "toolCallId", "call_id"];

fn str_key<'a>(
    o: &'a serde_json::Map<String, serde_json::Value>,
    keys: &[&str],
) -> Option<&'a str> {
    keys.iter().find_map(|k| o.get(*k).and_then(|v| v.as_str()))
}

/// Record the id of every object carrying a call id whose contents mention
/// `git commit` — i.e. the tool calls that committed.
fn record_commit_calls(v: &serde_json::Value, calls: &mut HashSet<String>) {
    match v {
        serde_json::Value::Object(o) => {
            if let Some(id) = str_key(o, &CALL_ID_KEYS) {
                let mut mentions = false;
                visit_strings(v, &mut |s| mentions |= s.contains("git commit"));
                if mentions {
                    calls.insert(id.to_string());
                }
            }
            o.values().for_each(|x| record_commit_calls(x, calls));
        }
        serde_json::Value::Array(a) => a.iter().for_each(|x| record_commit_calls(x, calls)),
        _ => {}
    }
}

/// Call `f` on every string inside the results of the tool calls in `calls`.
fn visit_commit_results(v: &serde_json::Value, calls: &HashSet<String>, f: &mut impl FnMut(&str)) {
    match v {
        serde_json::Value::Object(o) => {
            if str_key(o, &RESULT_ID_KEYS).is_some_and(|id| calls.contains(id)) {
                visit_strings(v, f);
            } else {
                o.values().for_each(|x| visit_commit_results(x, calls, f));
            }
        }
        serde_json::Value::Array(a) => a.iter().for_each(|x| visit_commit_results(x, calls, f)),
        _ => {}
    }
}

/// Shas leading `git log --oneline` lines: `<sha> <subject>`.
fn oneline_shas_in(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            let (sha, rest) = line.trim_start().split_once(' ')?;
            (is_sha(sha) && !rest.trim().is_empty()).then(|| sha.to_string())
        })
        .collect()
}

fn is_sha(s: &str) -> bool {
    (7..=40).contains(&s.len()) && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Shas from `git commit` output lines: `[<branch> <sha>] <subject>`, also
/// `[<branch> (root-commit) <sha>]` and `[detached HEAD <sha>]`. A match may be
/// quoted text rather than real output, so candidates must also exist in the
/// repository and fall within a session's time span.
fn commit_shas_in(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let Some(rest) = line.trim_start().strip_prefix('[') else {
            continue;
        };
        let Some(close) = rest.find("] ") else {
            continue;
        };
        let mut tokens = rest[..close].split_whitespace();
        let (Some(_branch), Some(sha)) = (tokens.next(), tokens.next_back()) else {
            continue;
        };
        if is_sha(sha) {
            out.push(sha.to_string());
        }
    }
    out
}

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "core.quotepath=false"])
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn git_toplevel(worktree: &Path) -> Option<PathBuf> {
    let top = git(worktree, &["rev-parse", "--show-toplevel"])?;
    Some(PathBuf::from(top.trim_end()))
}

/// Metadata for `sha` in the repo at `dir`. `None` for unknown shas (e.g. a
/// commit made in another repository) and merge commits.
fn commit_meta(dir: &Path, sha: &str) -> Option<CommitMeta> {
    let rev = format!("{sha}^{{commit}}");
    let meta = git(dir, &["show", "-s", "--format=%H%x00%P%x00%ct%x00%s", &rev])?;
    let mut parts = meta.trim_end_matches('\n').splitn(4, '\0');
    let full = parts.next()?.to_string();
    let parents = parts.next()?;
    let ts: i64 = parts.next()?.parse().ok()?;
    let subject = parts.next().unwrap_or("").to_string();
    if parents.split_whitespace().count() > 1 {
        return None;
    }
    Some(CommitMeta {
        full,
        parent: parents.trim().to_string(),
        ts_ms: ts * 1000,
        subject,
    })
}

/// Load the patch for a commit whose metadata is known.
fn load_commit(dir: &Path, m: CommitMeta) -> Option<Commit> {
    let patch = git(
        dir,
        &[
            "diff-tree",
            "-p",
            "--root",
            "--no-commit-id",
            "--no-color",
            "--no-ext-diff",
            "--no-renames",
            // One context line: enough to anchor the hunk in the file, and close to
            // an Edit call's old/new shape (sessionx counts context as changed).
            "-U1",
            &m.full,
        ],
    )?;
    Some(Commit {
        full: m.full,
        parent: m.parent,
        ts_ms: m.ts_ms,
        subject: m.subject,
        files: parse_patch(&patch),
        patch_id: patch_id(&patch),
    })
}

/// Decode a `---`/`+++` path as git writes it, then strip its `a/`/`b/`
/// prefix. Git C-quotes a path containing `"`, `\` or control characters
/// (`"b/a\tb.txt"`, octal `\ooo` for raw bytes); an unquoted path containing a
/// space is followed by a tab delimiter, which isn't part of the name.
fn patch_path(raw: &str, prefix: &str) -> String {
    let decoded = match raw.strip_prefix('"') {
        Some(quoted) => unquote_c(quoted),
        None => raw.strip_suffix('\t').unwrap_or(raw).to_string(),
    };
    match decoded.strip_prefix(prefix) {
        Some(rest) => rest.to_string(),
        None => decoded,
    }
}

/// Decode the body of a C-quoted git path (after the opening `"`), stopping at
/// the closing quote.
fn unquote_c(quoted: &str) -> String {
    let mut bytes = Vec::new();
    let mut it = quoted.bytes().peekable();
    while let Some(b) = it.next() {
        match b {
            b'"' => break,
            b'\\' => match it.next() {
                Some(b'a') => bytes.push(0x07),
                Some(b'b') => bytes.push(0x08),
                Some(b't') => bytes.push(b'\t'),
                Some(b'n') => bytes.push(b'\n'),
                Some(b'v') => bytes.push(0x0b),
                Some(b'f') => bytes.push(0x0c),
                Some(b'r') => bytes.push(b'\r'),
                Some(d @ b'0'..=b'7') => {
                    let mut v = u32::from(d - b'0');
                    for _ in 0..2 {
                        match it.peek() {
                            Some(&n @ b'0'..=b'7') => {
                                v = v * 8 + u32::from(n - b'0');
                                it.next();
                            }
                            _ => break,
                        }
                    }
                    bytes.push(v as u8);
                }
                Some(other) => bytes.push(other), // \" and \\
                None => break,
            },
            _ => bytes.push(b),
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Parse a unified `git diff` patch into per-file changes. Binary files are
/// skipped. Hunk bodies are consumed by their header's line counts, so a
/// removed line that itself starts with `--` can't be mistaken for a header.
fn parse_patch(text: &str) -> Vec<FileDiff> {
    #[derive(Default)]
    struct Cur {
        old_path: Option<String>,
        new_path: Option<String>,
        added: bool,
        deleted: bool,
        binary: bool,
        hunks: Vec<(Vec<String>, Vec<String>)>,
    }
    fn finish(cur: Cur, out: &mut Vec<FileDiff>) {
        if cur.binary {
            return;
        }
        // A whole-file add/delete: every hunk's one side, concatenated.
        let whole = |new_side: bool| {
            cur.hunks
                .iter()
                .flat_map(|(o, n)| if new_side { n } else { o }.iter().cloned())
                .collect::<Vec<_>>()
                .join("\n")
        };
        let (path, change) = if cur.added {
            (cur.new_path, FileChange::Added(whole(true)))
        } else if cur.deleted {
            (cur.old_path, FileChange::Deleted(whole(false)))
        } else if cur.hunks.is_empty() {
            return; // mode-only change
        } else {
            let hunks = cur
                .hunks
                .iter()
                .map(|(o, n)| (o.join("\n"), n.join("\n")))
                .collect();
            (cur.new_path, FileChange::Modified(hunks))
        };
        if let Some(path) = path {
            out.push(FileDiff { path, change });
        }
    }
    fn count(spec: &str) -> usize {
        // "-12,3" / "+4" -> 3 / 1
        spec[1..]
            .split_once(',')
            .map_or(Some(1), |(_, n)| n.parse().ok())
            .unwrap_or(0)
    }

    let mut out = Vec::new();
    let mut cur: Option<Cur> = None;
    let (mut old_left, mut new_left) = (0usize, 0usize);
    for line in text.lines() {
        if old_left > 0 || new_left > 0 {
            let Some(c) = cur.as_mut() else { break };
            let Some(h) = c.hunks.last_mut() else { break };
            match line.as_bytes().first() {
                Some(b'-') => {
                    h.0.push(line[1..].to_string());
                    old_left = old_left.saturating_sub(1);
                }
                Some(b'+') => {
                    h.1.push(line[1..].to_string());
                    new_left = new_left.saturating_sub(1);
                }
                Some(b'\\') => {} // "\ No newline at end of file"
                _ => {
                    // Context (an empty line is context whose space was trimmed).
                    let ctx = line.get(1..).unwrap_or("").to_string();
                    h.0.push(ctx.clone());
                    h.1.push(ctx);
                    old_left = old_left.saturating_sub(1);
                    new_left = new_left.saturating_sub(1);
                }
            }
            continue;
        }
        if line.starts_with("diff --git ") {
            if let Some(c) = cur.take() {
                finish(c, &mut out);
            }
            cur = Some(Cur::default());
            continue;
        }
        let Some(c) = cur.as_mut() else { continue };
        if line.starts_with("new file mode") {
            c.added = true;
        } else if line.starts_with("deleted file mode") {
            c.deleted = true;
        } else if line.starts_with("Binary files ") || line == "GIT binary patch" {
            c.binary = true;
        } else if let Some(p) = line.strip_prefix("--- ") {
            c.old_path = (p != "/dev/null").then(|| patch_path(p, "a/"));
        } else if let Some(p) = line.strip_prefix("+++ ") {
            c.new_path = (p != "/dev/null").then(|| patch_path(p, "b/"));
        } else if let Some(rest) = line.strip_prefix("@@ ") {
            let mut specs = rest.split_whitespace();
            old_left = specs.next().map_or(0, count);
            new_left = specs.next().map_or(0, count);
            c.hunks.push((Vec::new(), Vec::new()));
        }
    }
    if let Some(c) = cur {
        finish(c, &mut out);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn commit_shas_in_matches_commit_output_forms() {
        let text = "pre-commit ok\n\
                    [bakedbean/foo 05ec84a] feat: x\n 3 files changed\n\
                    [main (root-commit) 1234567abc] init\n\
                    [detached HEAD deadbeef] wip\n\
                    [not a sha] nope\n\
                    [x ABCDEF1] uppercase is not git's format\n\
                    [solo1234567] no branch";
        assert_eq!(
            commit_shas_in(text),
            vec!["05ec84a", "1234567abc", "deadbeef"]
        );
    }

    #[test]
    fn oneline_shas_come_only_from_commit_call_results() {
        let call = serde_json::json!({"type": "assistant", "message": {"content": [
            {"type": "tool_use", "id": "t1", "input": {"command": "git commit -q -m x && git log --oneline -2"}},
            {"type": "tool_use", "id": "t2", "input": {"command": "git log --oneline -1"}},
        ]}});
        let results = serde_json::json!({"type": "user", "message": {"content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": "hook noise\n936370cff feat: x\nabc1234 older"},
            {"type": "tool_result", "tool_use_id": "t2", "content": "1111111 not from a commit call"},
        ]}});
        let mut calls = HashSet::new();
        record_commit_calls(&call, &mut calls);
        assert!(calls.contains("t1") && !calls.contains("t2"), "{calls:?}");
        let mut found = Vec::new();
        visit_commit_results(&results, &calls, &mut |s| found.extend(oneline_shas_in(s)));
        assert_eq!(found, vec!["936370cff", "abc1234"]);
    }

    #[test]
    fn parse_patch_modified_added_deleted_binary() {
        let patch = "\
diff --git a/src/a.rs b/src/a.rs
index 1111111..2222222 100644
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,3 +1,3 @@
 fn a() {
--- not a header, a removed line
+++ not a header, an added line
 }
@@ -10 +10,2 @@ fn tail()
-x
+y
+z
diff --git a/new.txt b/new.txt
new file mode 100644
index 0000000..3333333
--- /dev/null
+++ b/new.txt
@@ -0,0 +1,2 @@
+hello
+world
\\ No newline at end of file
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
index 4444444..0000000
--- a/gone.txt
+++ /dev/null
@@ -1 +0,0 @@
-bye
diff --git a/img.png b/img.png
index 5555555..6666666 100644
Binary files a/img.png and b/img.png differ
";
        assert_eq!(
            parse_patch(patch),
            vec![
                FileDiff {
                    path: "src/a.rs".into(),
                    change: FileChange::Modified(vec![
                        (
                            "fn a() {\n-- not a header, a removed line\n}".into(),
                            "fn a() {\n++ not a header, an added line\n}".into()
                        ),
                        ("x".into(), "y\nz".into()),
                    ]),
                },
                FileDiff {
                    path: "new.txt".into(),
                    change: FileChange::Added("hello\nworld".into()),
                },
                FileDiff {
                    path: "gone.txt".into(),
                    change: FileChange::Deleted("bye".into()),
                },
            ]
        );
    }

    #[test]
    fn patch_path_decodes_git_path_formats() {
        assert_eq!(patch_path("b/src/a.rs", "b/"), "src/a.rs");
        // Unquoted path with a space: git appends a tab delimiter.
        assert_eq!(patch_path("b/a file.txt\t", "b/"), "a file.txt");
        // Quoted path: escapes decoded, prefix inside the quotes.
        assert_eq!(patch_path("\"b/a\\tb.txt\"", "b/"), "a\tb.txt");
        assert_eq!(patch_path("\"a/q\\\"x\\\\y\"", "a/"), "q\"x\\y");
        // Octal bytes form UTF-8 (core.quotepath=true style).
        assert_eq!(patch_path("\"b/caf\\303\\251\"", "b/"), "café");
    }

    #[test]
    fn refresh_resolves_paths_with_spaces_and_tabs() {
        let (_dir, repo, _) = fixture();
        std::fs::write(repo.join("a file.txt"), "x\n").unwrap();
        std::fs::write(repo.join("t\tb.txt"), "y\n").unwrap();
        run_git(&repo, &["add", "."]);
        let out = run_git(&repo, &["commit", "-m", "odd names"]);
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let log = repo.join("odd.jsonl");
        let line = serde_json::json!({"timestamp": now, "o": out});
        std::fs::write(&log, format!("{line}\n")).unwrap();
        let evs = CommitIndex::default().refresh(&repo, &[log], &[]);
        let mut paths: Vec<_> = evs.iter().map(|e| e.file_path.clone()).collect();
        paths.sort();
        paths.dedup();
        assert!(paths.contains(&repo.join("a file.txt")), "{paths:?}");
        assert!(paths.contains(&repo.join("t\tb.txt")), "{paths:?}");
    }

    // ── against a real repository ─────────────────────────────────────────

    fn run_git(dir: &Path, args: &[&str]) -> String {
        run_git_at(dir, None, args)
    }

    /// `run_git`, optionally committing at `date` (author and committer).
    fn run_git_at(dir: &Path, date: Option<&str>, args: &[&str]) -> String {
        let mut cmd = Command::new("git");
        if let Some(d) = date {
            cmd.env("GIT_AUTHOR_DATE", d).env("GIT_COMMITTER_DATE", d);
        }
        let out = cmd
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// A repo whose second commit rewrites `a.txt` and adds `b.txt`, plus a
    /// session log reporting that commit the way Claude Code's Bash tool does.
    fn fixture() -> (tempfile::TempDir, PathBuf, String) {
        let dir = tempfile::tempdir().unwrap();
        let repo = std::fs::canonicalize(dir.path()).unwrap();
        run_git(&repo, &["init", "-q"]);
        std::fs::write(repo.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        run_git(&repo, &["add", "."]);
        // Made long before the session, so only the second commit is its work.
        run_git_at(
            &repo,
            Some("2001-01-01T00:00:00Z"),
            &["commit", "-q", "-m", "base"],
        );
        std::fs::write(repo.join("a.txt"), "one\nTWO\nthree\n").unwrap();
        std::fs::write(repo.join("b.txt"), "new\n").unwrap();
        run_git(&repo, &["add", "."]);
        let out = run_git(&repo, &["commit", "-m", "shell edits"]);
        let log = repo.join("session.jsonl");
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let line = serde_json::json!({
            "type": "user",
            "timestamp": now,
            "message": {"content": [{"type": "tool_result", "content": out}]},
        });
        let mut f = std::fs::File::create(&log).unwrap();
        writeln!(f, "{line}").unwrap();
        (dir, repo, log.display().to_string())
    }

    #[test]
    fn refresh_reconstructs_committed_shell_edits() {
        let (_dir, repo, log) = fixture();
        let mut idx = CommitIndex::default();
        let evs = idx.refresh(&repo, &[PathBuf::from(&log)], &[]);
        assert_eq!(evs.len(), 2, "{evs:#?}");
        let a = evs
            .iter()
            .find(|e| e.file_path == repo.join("a.txt"))
            .unwrap();
        assert_eq!(a.tool, ChangeTool::Edit);
        assert_eq!(
            a.detail,
            ChangeDetail::Edit {
                old: "one\ntwo\nthree".into(),
                new: "one\nTWO\nthree".into(),
            }
        );
        assert!(a.summary.ends_with(" shell edits"), "{}", a.summary);
        assert!(a.source.session_file.to_string_lossy().starts_with("git:"));
        let b = evs
            .iter()
            .find(|e| e.file_path == repo.join("b.txt"))
            .unwrap();
        assert_eq!(b.tool, ChangeTool::Write);
        assert_eq!(b.detail, ChangeDetail::Write { head: "new".into() });
    }

    #[test]
    fn refresh_skips_files_an_edit_tool_already_recorded() {
        let (_dir, repo, log) = fixture();
        let mut idx = CommitIndex::default();
        let commit_ts = idx.refresh(&repo, &[PathBuf::from(&log)], &[])[0].timestamp_ms;
        let tool_edit = ChangeEvent {
            timestamp_ms: commit_ts - 1,
            tool: ChangeTool::Edit,
            file_path: repo.join("a.txt"),
            summary: String::new(),
            detail: ChangeDetail::None,
            source: ChangeSource::default(),
        };
        let evs = idx.refresh(&repo, &[PathBuf::from(&log)], &[tool_edit]);
        let files: Vec<_> = evs.iter().map(|e| e.file_path.clone()).collect();
        assert_eq!(files, vec![repo.join("b.txt")]);
    }

    #[test]
    fn refresh_finds_quiet_commits_through_the_reflog() {
        let (_dir, repo, _) = fixture();
        std::fs::write(repo.join("a.txt"), "one\nTWO\nthree\nfour\n").unwrap();
        run_git(&repo, &["commit", "-q", "-am", "quiet"]);
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        // A session that spans "now" but never printed commit output.
        let log = repo.join("quiet.jsonl");
        std::fs::write(&log, format!("{{\"timestamp\":\"{now}\"}}\n")).unwrap();
        let evs = CommitIndex::default().refresh(&repo, &[log], &[]);
        assert!(
            evs.iter().any(|e| e.summary.ends_with(" quiet")),
            "{evs:#?}"
        );
    }

    #[test]
    fn refresh_finds_commits_listed_after_a_quiet_commit_without_reflog() {
        let (_dir, repo, _) = fixture();
        std::fs::write(repo.join("a.txt"), "one\nTWO\nthree\nfour\n").unwrap();
        run_git(&repo, &["commit", "-q", "-am", "quiet"]);
        let sha = run_git(&repo, &["rev-parse", "--short", "HEAD"]);
        // Lose the reflog, as a recreated worktree does.
        std::fs::remove_dir_all(repo.join(".git/logs")).unwrap();
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let call = serde_json::json!({"timestamp": now, "message": {"content": [
            {"type": "tool_use", "id": "c1", "input": {"command": "git commit -q -m quiet && git log --oneline -1"}}]}});
        let result = serde_json::json!({"timestamp": now, "message": {"content": [
            {"type": "tool_result", "tool_use_id": "c1", "content": format!("{} quiet", sha.trim())}]}});
        let log = repo.join("quiet.jsonl");
        std::fs::write(&log, format!("{call}\n{result}\n")).unwrap();
        let evs = CommitIndex::default().refresh(&repo, &[log], &[]);
        assert!(
            evs.iter().any(|e| e.summary.ends_with(" quiet")),
            "{evs:#?}"
        );
    }

    #[test]
    fn refresh_ignores_oneline_commits_outside_every_session() {
        let (_dir, repo, _) = fixture();
        std::fs::remove_dir_all(repo.join(".git/logs")).unwrap();
        let sha = run_git(&repo, &["rev-parse", "--short", "HEAD"]);
        let ts = "2020-01-01T00:00:00.000Z";
        let call = serde_json::json!({"timestamp": ts, "message": {"content": [
            {"type": "tool_use", "id": "c1", "input": {"command": "git commit && git log --oneline"}}]}});
        let result = serde_json::json!({"timestamp": ts, "message": {"content": [
            {"type": "tool_result", "tool_use_id": "c1", "content": format!("{} shell edits", sha.trim())}]}});
        let log = repo.join("old.jsonl");
        std::fs::write(&log, format!("{call}\n{result}\n")).unwrap();
        assert!(
            CommitIndex::default()
                .refresh(&repo, &[log], &[])
                .is_empty()
        );
    }

    #[test]
    fn refresh_ignores_reflog_commits_outside_every_session() {
        let (_dir, repo, _) = fixture();
        let log = repo.join("old.jsonl");
        std::fs::write(&log, "{\"timestamp\":\"2020-01-01T00:00:00.000Z\"}\n").unwrap();
        assert!(
            CommitIndex::default()
                .refresh(&repo, &[log], &[])
                .is_empty()
        );
    }

    #[test]
    fn refresh_collapses_a_rebased_copy_into_its_original() {
        let (_dir, repo, log) = fixture();
        // Replay the same change onto a different parent, as a rebase does.
        let original = run_git(&repo, &["rev-parse", "HEAD"]);
        run_git(&repo, &["checkout", "-q", "-b", "other", "HEAD~1"]);
        std::fs::write(repo.join("c.txt"), "unrelated\n").unwrap();
        run_git(&repo, &["add", "c.txt"]);
        run_git(&repo, &["commit", "-q", "-m", "unrelated"]);
        let out = run_git(&repo, &["cherry-pick", original.trim()]);
        let mut f = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
        let line = serde_json::json!({"o": out});
        writeln!(f, "{line}").unwrap();
        let evs = CommitIndex::default().refresh(&repo, &[PathBuf::from(&log)], &[]);
        let a_edits = evs
            .iter()
            .filter(|e| e.file_path == repo.join("a.txt"))
            .count();
        assert_eq!(a_edits, 1, "{evs:#?}");
    }

    #[test]
    fn refresh_ignores_quoted_commit_output_from_outside_every_session() {
        let (_dir, repo, _) = fixture();
        let sha = run_git(&repo, &["rev-parse", "--short", "HEAD"]);
        // A user pastes old commit output into a session that started later.
        let line = serde_json::json!({
            "timestamp": "2099-01-01T00:00:00.000Z",
            "message": {"content": format!("[main {}] look at this", sha.trim())},
        });
        let log = repo.join("paste.jsonl");
        std::fs::write(&log, format!("{line}\n")).unwrap();
        assert!(
            CommitIndex::default()
                .refresh(&repo, &[log], &[])
                .is_empty()
        );
    }

    #[test]
    fn refresh_reuses_its_result_until_a_log_grows() {
        let (_dir, repo, log) = fixture();
        let mut idx = CommitIndex::default();
        let log = PathBuf::from(&log);
        let first = idx.refresh(&repo, std::slice::from_ref(&log), &[]);
        assert!(!first.is_empty());
        // Losing the repo's objects can't change an unchanged log's result.
        std::fs::remove_dir_all(repo.join(".git")).unwrap();
        assert_eq!(idx.refresh(&repo, std::slice::from_ref(&log), &[]), first);
    }

    /// A session log spanning "now" with no commit output: commits are found
    /// through the reflog.
    fn quiet_log(repo: &Path) -> PathBuf {
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let log = repo.join("quiet.jsonl");
        std::fs::write(&log, format!("{{\"timestamp\":\"{now}\"}}\n")).unwrap();
        log
    }

    fn summaries(evs: &[ChangeEvent]) -> Vec<String> {
        let mut out: Vec<String> = evs
            .iter()
            .map(|e| e.summary.split_once(' ').unwrap().1.to_string())
            .collect();
        out.dedup();
        out
    }

    #[test]
    fn refresh_keeps_only_the_amended_version_within_one_second() {
        let (_dir, repo, _) = fixture();
        std::fs::write(repo.join("a.txt"), "draft\n").unwrap();
        run_git(&repo, &["commit", "-q", "-am", "draft"]);
        std::fs::write(repo.join("a.txt"), "final\n").unwrap();
        run_git(&repo, &["commit", "-q", "--amend", "-am", "final"]);
        let evs = CommitIndex::default().refresh(&repo, &[quiet_log(&repo)], &[]);
        let s = summaries(&evs);
        assert!(s.contains(&"final".to_string()), "{s:?}");
        assert!(!s.contains(&"draft".to_string()), "{s:?}");
    }

    #[test]
    fn refresh_keeps_sibling_commits_on_two_branches() {
        let (_dir, repo, _) = fixture();
        run_git(&repo, &["checkout", "-q", "-b", "left"]);
        std::fs::write(repo.join("l.txt"), "l\n").unwrap();
        run_git(&repo, &["add", "l.txt"]);
        run_git(&repo, &["commit", "-q", "-m", "left work"]);
        run_git(&repo, &["checkout", "-q", "-b", "right", "HEAD~1"]);
        std::fs::write(repo.join("r.txt"), "r\n").unwrap();
        run_git(&repo, &["add", "r.txt"]);
        run_git(&repo, &["commit", "-q", "-m", "right work"]);
        let evs = CommitIndex::default().refresh(&repo, &[quiet_log(&repo)], &[]);
        let s = summaries(&evs);
        assert!(s.contains(&"left work".to_string()), "{s:?}");
        assert!(s.contains(&"right work".to_string()), "{s:?}");
    }

    #[test]
    fn refresh_keeps_a_change_reapplied_after_its_revert() {
        let (_dir, repo, _) = fixture();
        let head = run_git(&repo, &["rev-parse", "HEAD"]);
        run_git(&repo, &["revert", "--no-edit", head.trim()]);
        run_git(&repo, &["cherry-pick", head.trim()]);
        // cherry-pick and revert aren't `commit` reflog entries; report them
        // the way an agent's `git log --oneline` check would.
        let log = repo.join("s.jsonl");
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let shas = run_git(&repo, &["log", "--format=%h %s", "-3"]);
        let call = serde_json::json!({"timestamp": now, "message": {"content": [
            {"type": "tool_use", "id": "c", "input": {"command": "git commit; git log --oneline -3"}}]}});
        let result = serde_json::json!({"timestamp": now, "message": {"content": [
            {"type": "tool_result", "tool_use_id": "c", "content": shas}]}});
        std::fs::write(&log, format!("{call}\n{result}\n")).unwrap();
        let evs = CommitIndex::default().refresh(&repo, &[log], &[]);
        let a = evs
            .iter()
            .filter(|e| e.file_path == repo.join("a.txt"))
            .count();
        assert_eq!(a, 3, "apply, revert, reapply: {:?}", summaries(&evs));
    }

    #[test]
    fn refresh_ignores_unknown_shas_and_non_repos() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("s.jsonl");
        std::fs::write(&log, "{\"x\":\"[main abc1234] not here\"}\n").unwrap();
        let mut idx = CommitIndex::default();
        assert!(idx.refresh(dir.path(), &[log], &[]).is_empty());
    }

    #[test]
    fn scan_log_is_incremental_and_waits_for_complete_lines() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("s.jsonl");
        std::fs::write(&log, "{\"o\":\"[m 1111111] a\"}\n{\"o\":\"[m 2222222] b\"}").unwrap();
        let mut scan = LogScan::default();
        scan_log(&log, &mut scan);
        assert_eq!(
            scan.shas,
            vec!["1111111"],
            "partial last line not consumed yet"
        );
        let mut f = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
        writeln!(f).unwrap();
        scan_log(&log, &mut scan);
        assert_eq!(scan.shas, vec!["1111111", "2222222"]);
    }
}
