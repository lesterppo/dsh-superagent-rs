//! Real-life layer: config, persistent state, GitHub polling via the
//! `gh` CLI, git-worktree work, and explicit-only outward actions.
//!
//! Safety defaults, by design (see README "Real-life use"):
//! - Polling is dry-run unless `--run-workers` is passed: it only
//!   records what would be accepted.
//! - Work never pushes, never creates a PR, never merges, unless
//!   the matching explicit flag is passed on that exact command.
//! - Only explicit requests from allowed users are accepted
//!   (RuleAssessor accepts Explicit, ignores Inferred).
//! - No Feishu adapter, no panel, no DSH sessions: those remain
//!   deployment-specific and out of scope.

use crate::engine::{Engine, RuleAssessor};
use crate::model::{now_iso, AcceptMode, ActorRef, ThreadRef, Trigger};
use crate::policy::{Allowed, Settings, SourcePolicy};
use crate::store::Store;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Deployment config, stored as JSON (see `init`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RealConfig {
    #[serde(default)]
    pub repo: String, // "owner/repo" for poll/PR; empty = local work only
    #[serde(default)]
    pub allowed: Vec<String>, // GitHub logins allowed to task the agent
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default = "default_work_root")]
    pub work_root: String,
    #[serde(default = "default_state")]
    pub state_file: String,
    #[serde(default = "default_interval")]
    pub poll_interval_seconds: u64,
    #[serde(default)]
    pub blocked_commands: Vec<String>,
    #[serde(default = "default_turns")]
    pub max_turns: usize,
}
fn default_model() -> String {
    "deepseek".into()
}
fn default_work_root() -> String {
    "superagent-work".into()
}
fn default_state() -> String {
    "superagent-state.json".into()
}
fn default_interval() -> u64 {
    300
}
fn default_turns() -> usize {
    12
}
impl Default for RealConfig {
    fn default() -> Self {
        RealConfig {
            repo: String::new(),
            allowed: vec![],
            model: default_model(),
            work_root: default_work_root(),
            state_file: default_state(),
            poll_interval_seconds: default_interval(),
            blocked_commands: vec!["rm".into()],
            max_turns: default_turns(),
        }
    }
}

pub fn default_config_json() -> String {
    serde_json::to_string_pretty(&RealConfig::default()).unwrap()
}

pub fn load_config(path: &str) -> Result<RealConfig, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("read config {path}: {e}"))?;
    serde_json::from_str(&text).map_err(|e| format!("parse config {path}: {e}"))
}

/// Settings for the engine, from a real config. Only the github
/// source is enabled, with the allowed list; eval mode is Observe
/// (inferred requests are never auto-accepted by RuleAssessor).
pub fn build_settings(cfg: &RealConfig) -> Settings {
    let mut s = Settings::default();
    s.eval_mode = AcceptMode::Observe;
    s.blocked_commands = cfg.blocked_commands.clone();
    let mut src = SourcePolicy::default();
    src.enabled = true;
    for id in &cfg.allowed {
        src.allowed.push(Allowed { id: id.clone(), name: None });
    }
    s.sources.insert("github".into(), src);
    s
}

pub fn load_store(path: &str) -> Result<Store, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| format!("parse state {path}: {e}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Store::default()),
        Err(e) => Err(format!("read state {path}: {e}")),
    }
}

pub fn save_store(store: &Store, path: &str) -> Result<(), String> {
    if let Some(parent) = Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| format!("create state dir: {e}"))?;
        }
    }
    let text = serde_json::to_string_pretty(store).map_err(|e| format!("serialize state: {e}"))?;
    std::fs::write(path, text).map_err(|e| format!("write state {path}: {e}"))
}

pub fn load_engine(cfg: &RealConfig) -> Result<Engine, String> {
    let mut eng = Engine::new(build_settings(cfg));
    eng.store = load_store(&cfg.state_file)?;
    Ok(eng)
}

pub fn save_engine(eng: &Engine, cfg: &RealConfig) -> Result<(), String> {
    save_store(&eng.store, &cfg.state_file)
}

pub fn status_value(eng: &Engine) -> serde_json::Value {
    let tickets: Vec<serde_json::Value> = eng
        .store
        .tickets
        .values()
        .map(|t| {
            serde_json::json!({
                "id": t.id, "title": t.title,
                "ended": t.ended.as_ref().map(|e| e.outcome.clone()),
                "attention": t.attention.as_ref().map(|a| a.question.clone()),
                "assignments": t.assignments.len(),
                "updated_at": t.updated_at,
            })
        })
        .collect();
    serde_json::json!({
        "tickets": tickets,
        "threads": eng.store.threads.len(),
        "triggers": eng.store.triggers.len(),
        "decisions": eng.store.decisions.len(),
        "outbox": eng.outbox.len(),
        "invariants": eng.store.check_invariants(),
    })
}

/// Run a command, returning stdout on success, else a combined error.
pub fn run_cmd(program: &str, args: &[&str], cwd: Option<&Path>) -> Result<String, String> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let out = cmd.output().map_err(|e| format!("spawn {program}: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    if out.status.success() {
        Ok(stdout)
    } else {
        Err(format!("{program} {args:?} failed: {stderr}{stdout}"))
    }
}

/// Parse `gh issue list --json ...` output into triggers.
/// Only issues that mention the agent account or carry the
/// `superagent` label become triggers; everything else is skipped
/// (not recorded, not evaluated). A label counts as addressed for
/// allowed users, same as a mention — a documented deployment rule.
pub fn parse_gh_issues(value: &serde_json::Value, own_login: &str) -> Vec<Trigger> {
    let mut out = vec![];
    let Some(arr) = value.as_array() else {
        return out;
    };
    for issue in arr {
        let number = issue.get("number").and_then(|n| n.as_u64()).unwrap_or(0);
        if number == 0 {
            continue;
        }
        let title = issue.get("title").and_then(|t| t.as_str()).unwrap_or("").to_string();
        let body = issue.get("body").and_then(|b| b.as_str()).unwrap_or("").to_string();
        let mention = format!("@{own_login}");
        let mentioned = title.contains(&mention) || body.contains(&mention);
        let labels: Vec<String> = issue
            .get("labels")
            .and_then(|l| l.as_array())
            .map(|xs| {
                xs.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()).or_else(|| x.get("name").and_then(|n| n.as_str()).map(|s| s.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        let labelled = labels.iter().any(|l| l == "superagent");
        if !mentioned && !labelled {
            continue;
        }
        let author = match issue.get("author") {
            Some(a) if a.is_string() => a.as_str().unwrap_or("").to_string(),
            Some(a) => a.get("login").and_then(|l| l.as_str()).unwrap_or("").to_string(),
            None => String::new(),
        };
        let human = !author.ends_with("[bot]");
        let text = if body.is_empty() { title.clone() } else { format!("{title}\n\n{body}") };
        out.push(Trigger {
            id: format!("gh-issue-{number}"),
            source: "github".into(),
            threads: vec![ThreadRef { source: "github".into(), id: format!("issue-{number}") }],
            actor: ActorRef { source: "github".into(), id: author },
            human,
            addressed: if mentioned { Some("mention".into()) } else { Some("label".into()) },
            at: now_iso(),
            text,
            channel: None,
            url: issue.get("url").and_then(|u| u.as_str()).map(|s| s.to_string()),
            meta: serde_json::json!({"number": number, "title": title}),
        });
    }
    out
}

/// Receive triggers that are new, then tick once. Returns
/// (decisions, new_trigger_count).
pub fn process_issues(eng: &mut Engine, triggers: Vec<Trigger>) -> (Vec<crate::model::Decision>, usize) {
    let mut fresh = 0usize;
    for t in triggers {
        if eng.store.triggers.contains_key(&t.id) {
            continue;
        }
        if eng.receive(t).is_ok() {
            fresh += 1;
        }
    }
    let mut assessor = RuleAssessor;
    let decisions = eng.tick(&mut assessor);
    (decisions, fresh)
}

/// Create a git worktree for a branch under work_root. Refuses if
/// the target path already exists, so work is never overwritten.
pub fn create_worktree(repo_path: &Path, work_root: &Path, branch: &str) -> Result<PathBuf, String> {
    // Absolute paths throughout: a relative worktree target is
    // resolved by git against the repo dir, while the worker's
    // file tools resolve it against the process cwd — a mismatch
    // that silently edits a non-git directory (caught by the
    // 2026-11-03 e2e test, which is why this is absolute here).
    let repo_abs = std::fs::canonicalize(repo_path).unwrap_or_else(|_| repo_path.to_path_buf());
    if !repo_abs.join(".git").exists() {
        return Err(format!("{} is not a git repo (no .git)", repo_abs.display()));
    }
    let root_abs = if work_root.is_absolute() {
        work_root.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(work_root)
    };
    std::fs::create_dir_all(&root_abs).map_err(|e| format!("create work root: {e}"))?;
    let root_abs = std::fs::canonicalize(&root_abs).unwrap_or(root_abs);
    let target = root_abs.join(branch);
    if target.exists() {
        return Err(format!("worktree target already exists: {}", target.display()));
    }
    run_cmd("git", &["-C", &repo_abs.to_string_lossy(), "worktree", "add", &target.to_string_lossy(), "-b", branch], None)?;
    Ok(target)
}

pub fn diff_stat(worktree: &Path) -> (String, String) {
    let status = run_cmd("git", &["-C", &worktree.to_string_lossy(), "status", "--short"], None).unwrap_or_else(|e| e);
    let stat = run_cmd("git", &["-C", &worktree.to_string_lossy(), "diff", "--stat"], None).unwrap_or_else(|e| e);
    (status, stat)
}

/// Commit all changes in a worktree. Returns "nothing to commit"
/// instead of an empty commit when there is no diff.
pub fn commit_worktree(worktree: &Path, message: &str) -> Result<String, String> {
    run_cmd("git", &["-C", &worktree.to_string_lossy(), "add", "-A"], None)?;
    let staged = run_cmd("git", &["-C", &worktree.to_string_lossy(), "diff", "--cached", "--quiet"], None);
    if staged.is_ok() {
        return Ok("nothing to commit".into());
    }
    run_cmd("git", &["-C", &worktree.to_string_lossy(), "commit", "-m", message], None)
}

/// Create a PR. Dry-run returns the command instead of running it.
/// Never merges: there is no merge flag anywhere in this codebase.
pub fn create_pr(repo: &str, head: &str, title: &str, body: &str, dry_run: bool) -> Result<String, String> {
    if dry_run {
        return Ok(format!("DRY-RUN: gh pr create --repo {repo} --head {head} --title {title:?}"));
    }
    run_cmd("gh", &["pr", "create", "--repo", repo, "--head", head, "--title", title, "--body", body], None)
}

/// Post an issue comment. Dry-run returns the command instead.
pub fn post_reply(repo: &str, issue_number: u64, text: &str, dry_run: bool) -> Result<String, String> {
    if dry_run {
        return Ok(format!("DRY-RUN: gh issue comment {issue_number} --repo {repo}"));
    }
    run_cmd("gh", &["issue", "comment", &issue_number.to_string(), "--repo", repo, "--body", text], None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn gh_issue(number: u64, title: &str, body: &str, author: &str, labels: Vec<&str>) -> serde_json::Value {
        serde_json::json!({
            "number": number, "title": title, "body": body,
            "author": {"login": author},
            "labels": labels.iter().map(|l| serde_json::json!({"name": l})).collect::<Vec<_>>(),
            "url": format!("https://github.com/o/r/issues/{number}"),
        })
    }

    #[test]
    fn parse_mention_label_and_skip() {
        let v = serde_json::json!([
            gh_issue(1, "Fix typo", "Hey @agent please fix", "alice", vec![]),
            gh_issue(2, "Labelled task", "Do this", "alice", vec!["superagent"]),
            gh_issue(3, "Unrelated", "No mention here", "alice", vec![]),
            gh_issue(4, "Bot task", "Hey @agent", "ci-bot[bot]", vec![]),
        ]);
        let ts = parse_gh_issues(&v, "agent");
        assert_eq!(ts.len(), 3);
        assert_eq!(ts[0].addressed.as_deref(), Some("mention"));
        assert_eq!(ts[1].addressed.as_deref(), Some("label"));
        assert!(!ts[2].human);
    }

    #[test]
    fn engine_accepts_allowed_mention_and_ignores_unlisted() {
        let mut cfg = RealConfig::default();
        cfg.allowed = vec!["alice".into()];
        let mut eng = Engine::new(build_settings(&cfg));
        let v = serde_json::json!([
            gh_issue(1, "Fix typo", "Hey @agent fix", "alice", vec![]),
            gh_issue(2, "Fix typo", "Hey @agent fix", "mallory", vec![]),
        ]);
        let triggers = parse_gh_issues(&v, "agent");
        let (decisions, fresh) = process_issues(&mut eng, triggers);
        assert_eq!(fresh, 2);
        assert_eq!(decisions.iter().filter(|d| d.kind == "accept").count(), 1);
        assert_eq!(eng.store.tickets.len(), 1);
        // Re-processing the same issues creates nothing new.
        let v2 = serde_json::json!([gh_issue(1, "Fix typo", "Hey @agent fix", "alice", vec![])]);
        let (_, fresh2) = process_issues(&mut eng, parse_gh_issues(&v2, "agent"));
        assert_eq!(fresh2, 0);
        assert_eq!(eng.store.tickets.len(), 1);
    }

    #[test]
    fn store_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut eng = Engine::new(build_settings(&RealConfig::default()));
        eng.receive(Trigger {
            id: "t1".into(),
            source: "github".into(),
            threads: vec![ThreadRef { source: "github".into(), id: "issue-1".into() }],
            actor: ActorRef { source: "github".into(), id: "alice".into() },
            human: true,
            addressed: Some("mention".into()),
            at: now_iso(),
            text: "work".into(),
            channel: None,
            url: None,
            meta: serde_json::json!({}),
        })
        .unwrap();
        save_store(&eng.store, path.to_str().unwrap()).unwrap();
        let back = load_store(path.to_str().unwrap()).unwrap();
        assert!(back.triggers.contains_key("t1"));
        assert_eq!(back.check_invariants(), Vec::<String>::new());
    }

    fn init_repo(dir: &Path) {
        run_cmd("git", &["init", "-q", &dir.to_string_lossy()], None).unwrap();
        run_cmd("git", &["-C", &dir.to_string_lossy(), "config", "user.email", "test@example.invalid"], None).unwrap();
        run_cmd("git", &["-C", &dir.to_string_lossy(), "config", "user.name", "Test"], None).unwrap();
        std::fs::write(dir.join("README.md"), "Hello Wrold\n").unwrap();
        run_cmd("git", &["-C", &dir.to_string_lossy(), "add", "-A"], None).unwrap();
        run_cmd("git", &["-C", &dir.to_string_lossy(), "commit", "-qm", "init"], None).unwrap();
    }

    #[test]
    fn worktree_diff_and_commit() {
        let dir = tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        let wt = create_worktree(&repo, &dir.path().join("work"), "agent/test").unwrap();
        std::fs::write(wt.join("README.md"), "Hello World\n").unwrap();
        let (status, stat) = diff_stat(&wt);
        assert!(status.contains("README.md"));
        assert!(stat.contains("README.md"));
        let out = commit_worktree(&wt, "Fix typo").unwrap();
        assert!(out.contains("Fix typo") || out.contains("1 file changed") || out.contains("master") || out.contains("main"));
        assert_eq!(commit_worktree(&wt, "again").unwrap(), "nothing to commit");
        // Refuses to overwrite an existing worktree target.
        assert!(create_worktree(&repo, &dir.path().join("work"), "agent/test").is_err());
    }

    #[test]
    fn post_reply_dry_run_does_not_call_gh() {
        let out = post_reply("o/r", 42, "hello", true).unwrap();
        assert!(out.contains("DRY-RUN"));
        assert!(out.contains("42"));
        let pr = create_pr("o/r", "agent/x", "T", "B", true).unwrap();
        assert!(pr.contains("DRY-RUN"));
    }
}
