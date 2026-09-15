//! What askfirst has seen before, and what you decided about it, per mode.
//!
//! A verdict belongs to the mode you gave it in. What you allowed as
//! `contributor` is not automatically allowed as `reader`, which is the point
//! of having modes at all. A verdict recorded under `*` applies in every mode.
//!
//! Only `askfirst review` writes a verdict. The hook appends observations and
//! nothing else, so the agent cannot widen its own permissions by running a
//! command: running it is exactly what puts it in the queue.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;

use crate::config::Action;

/// A verdict under this key applies whatever mode is active.
pub const ALL_MODES: &str = "*";

/// A command reduced to the granularity you review at: the program and its
/// first non-flag word, when that word is a plain identifier.
pub fn signature(argv: &[String]) -> String {
    let mut it = argv.iter();
    let Some(prog) = it.next() else { return String::new() };
    for word in it {
        if word.starts_with('-') {
            continue; // a flag is never the subcommand
        }
        // Only a plain identifier is treated as a subcommand. A path, a file
        // name, a glob or a variable is an argument, and folding those into
        // the signature would make the review queue endless.
        if is_subcommand_word(word) {
            return format!("{prog} {word}");
        }
        break;
    }
    prog.clone()
}

fn is_subcommand_word(w: &str) -> bool {
    !w.is_empty()
        && w.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && w.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == ':')
}

/// The program-only form of a signature, for when one verdict should cover
/// every subcommand.
pub fn broaden(sig: &str) -> String {
    sig.split_whitespace().next().unwrap_or(sig).to_string()
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Verdicts {
    #[serde(default)]
    pub modes: BTreeMap<String, ModeVerdicts>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ModeVerdicts {
    #[serde(default)]
    pub entries: BTreeMap<String, Entry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub action: Action,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided: Option<String>,
}

impl Verdicts {
    /// The verdict in force for this signature: the mode's own entry, then the
    /// mode's program-wide entry, then the same two under `*`.
    pub fn lookup(&self, mode: &str, sig: &str) -> Option<&Entry> {
        let broad = broaden(sig);
        for scope in [mode, ALL_MODES] {
            let Some(m) = self.modes.get(scope) else { continue };
            if let Some(e) = m.entries.get(sig) {
                return Some(e);
            }
            if let Some(e) = m.entries.get(&broad) {
                return Some(e);
            }
        }
        None
    }

    pub fn set(&mut self, mode: &str, sig: &str, entry: Entry) {
        self.modes
            .entry(mode.to_string())
            .or_default()
            .entries
            .insert(sig.to_string(), entry);
    }
}

/// One sighting of a signature the verdict file does not cover.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sighting {
    pub signature: String,
    pub example: String,
    /// The mode that was active when it came up, so review decides in context.
    #[serde(default)]
    pub mode: String,
}

/// The queue is JSON Lines: one object per line.
///
/// Every other file askfirst owns is a single JSON document. This one is
/// appended to by concurrent hook processes, and a JSON array cannot be
/// appended to without rewriting it, which would lose entries when two
/// sessions record at once.
fn read_pending() -> Vec<Sighting> {
    match std::fs::read_to_string(pending_path()) {
        Ok(t) => t
            .lines()
            .filter_map(|l| serde_json::from_str::<Sighting>(l).ok())
            .collect(),
        Err(_) => Vec::new(),
    }
}

pub fn verdicts_path() -> PathBuf {
    crate::config::dir().join("verdicts.json")
}

pub fn pending_path() -> PathBuf {
    crate::config::dir().join("pending.jsonl")
}

/// Global mode and the session the hook last ran for. One file rather than
/// two bare text files, so every file askfirst owns is TOML.
pub fn state_path() -> PathBuf {
    crate::config::dir().join("state.json")
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    /// The mode for every session that has not set its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// The session the hook saw most recently. The CLI has no other way to
    /// learn which session is asking.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_session: Option<String>,
}

pub fn load_state() -> State {
    match std::fs::read_to_string(state_path()) {
        Ok(t) => serde_json::from_str(&t).unwrap_or_default(),
        Err(_) => State::default(),
    }
}

fn save_state(st: &State) -> Result<(), String> {
    let path = state_path();
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
    }
    let body = serde_json::to_string_pretty(st).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, format!("{body}\n")).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

/// Modes set for one session, and the id of the session seen most recently.
///
/// The CLI has no way to learn its own session id: Claude Code does not put
/// one in the environment. The hook does get it on every event, and the hook
/// for the `askfirst mode` call itself runs immediately before that command,
/// so the last session the hook saw is the session asking.
pub fn sessions_path() -> PathBuf {
    crate::config::dir().join("sessions.json")
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Sessions {
    #[serde(default)]
    pub sessions: BTreeMap<String, SessionEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEntry {
    pub mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set: Option<String>,
}

pub fn load_sessions() -> Sessions {
    match std::fs::read_to_string(sessions_path()) {
        Ok(t) => serde_json::from_str(&t).unwrap_or_default(),
        Err(_) => Sessions::default(),
    }
}

pub fn set_session_mode(id: &str, mode: &str, stamp: &str) -> Result<(), String> {
    if id.is_empty() {
        return Err("no session to set the mode for".into());
    }
    let mut s = load_sessions();
    s.sessions.insert(
        id.to_string(),
        SessionEntry { mode: mode.to_string(), set: Some(stamp.to_string()) },
    );
    // Keep the file from growing without bound. Sessions end without telling
    // us, so the only signal is age.
    if s.sessions.len() > 200 {
        let drop: Vec<String> = s.sessions.keys().take(s.sessions.len() - 200).cloned().collect();
        for k in drop {
            s.sessions.remove(&k);
        }
    }
    let path = sessions_path();
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
    }
    let body = serde_json::to_string_pretty(&s).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, format!("{body}\n")).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

pub fn session_mode(id: &str) -> Option<String> {
    if id.is_empty() {
        return None;
    }
    load_sessions().sessions.get(id).map(|e| e.mode.clone())
}

/// Remember which session the hook last ran for, so the CLI can find it.
pub fn record_session(id: &str) {
    if id.is_empty() {
        return;
    }
    let mut st = load_state();
    if st.current_session.as_deref() == Some(id) {
        return; // unchanged, skip the write on every tool call
    }
    st.current_session = Some(id.to_string());
    let _ = save_state(&st);
}

pub fn last_session() -> Option<String> {
    load_state().current_session.filter(|s| !s.is_empty())
}

/// Which mode is in force: the environment, then this session's own mode,
/// then the machine-wide one, then the config's default.
pub fn active_mode_for(cfg: &crate::config::Config, session_id: &str) -> String {
    if let Ok(m) = std::env::var("ASKFIRST_MODE") {
        let m = m.trim().to_string();
        if !m.is_empty() {
            return m;
        }
    }
    if let Some(m) = session_mode(session_id) {
        return m;
    }
    if let Some(m) = load_state().mode.filter(|m| !m.is_empty()) {
        return m;
    }
    cfg.default_mode.clone().unwrap_or_default()
}

/// The mode the CLI should assume: the session the hook last ran for.
pub fn active_mode(cfg: &crate::config::Config) -> String {
    if let Ok(m) = std::env::var("ASKFIRST_MODE") {
        let m = m.trim().to_string();
        if !m.is_empty() {
            return m;
        }
    }
    if let Some(m) = last_session().and_then(|id| session_mode(&id)) {
        return m;
    }
    if let Some(m) = load_state().mode.filter(|m| !m.is_empty()) {
        return m;
    }
    cfg.default_mode.clone().unwrap_or_default()
}

pub fn set_mode(mode: &str) -> Result<(), String> {
    let mut st = load_state();
    st.mode = Some(mode.to_string());
    save_state(&st)
}

pub fn load_verdicts() -> Verdicts {
    match std::fs::read_to_string(verdicts_path()) {
        Ok(t) => serde_json::from_str(&t).unwrap_or_default(),
        Err(_) => Verdicts::default(),
    }
}

pub fn save_verdicts(v: &Verdicts) -> Result<(), String> {
    let path = verdicts_path();
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
    }
    let body = serde_json::to_string_pretty(v).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, format!("{body}\n")).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

/// Append a sighting. Best effort: a hook that cannot write its queue still
/// returns its decision rather than failing the tool call.
pub fn record_sighting(signature: &str, example: &str, mode: &str) {
    let path = pending_path();
    if let Some(d) = path.parent() {
        if std::fs::create_dir_all(d).is_err() {
            return;
        }
    }
    let line = match serde_json::to_string(&Sighting {
        signature: signature.to_string(),
        example: example.chars().take(400).collect(),
        mode: mode.to_string(),
    }) {
        Ok(l) => l,
        Err(_) => return,
    };
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{line}");
    }
}

/// Signatures seen in `mode` but not yet decided there, most-seen first.
pub fn pending(mode: &str) -> Vec<(String, String, usize)> {
    let verdicts = load_verdicts();
    let mut counts: BTreeMap<String, (String, usize)> = BTreeMap::new();
    for s in read_pending() {
        if s.mode != mode {
            continue;
        }
        if verdicts.lookup(mode, &s.signature).is_some() {
            continue;
        }
        let e = counts.entry(s.signature).or_insert((s.example.clone(), 0));
        e.1 += 1;
    }
    let mut v: Vec<(String, String, usize)> =
        counts.into_iter().map(|(k, (ex, n))| (k, ex, n)).collect();
    v.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
    v
}

/// Every mode that has something waiting, with its count.
pub fn pending_modes() -> Vec<(String, usize)> {
    let verdicts = load_verdicts();
    let mut per_mode: BTreeMap<String, std::collections::BTreeSet<String>> = BTreeMap::new();
    for s in read_pending() {
        if verdicts.lookup(&s.mode, &s.signature).is_some() {
            continue;
        }
        per_mode.entry(s.mode).or_default().insert(s.signature);
    }
    per_mode.into_iter().map(|(m, s)| (m, s.len())).collect()
}

/// Drop sightings that now have a verdict in the mode they were seen in.
pub fn prune_pending() -> Result<(), String> {
    let verdicts = load_verdicts();
    let path = pending_path();
    if !path.exists() {
        return Ok(());
    }
    let kept: Vec<Sighting> = read_pending()
        .into_iter()
        .filter(|s| verdicts.lookup(&s.mode, &s.signature).is_none())
        .collect();
    let mut body = String::new();
    for s in &kept {
        if let Ok(l) = serde_json::to_string(s) {
            body.push_str(&l);
            body.push('\n');
        }
    }
    let tmp = path.with_extension("jsonl.tmp");
    std::fs::write(&tmp, body).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_string).collect()
    }

    fn entry(a: Action) -> Entry {
        Entry { action: a, note: None, decided: None }
    }

    #[test]
    fn signature_is_program_plus_subcommand() {
        assert_eq!(signature(&argv("git push origin main")), "git push");
        assert_eq!(signature(&argv("cargo test --release")), "cargo test");
        assert_eq!(signature(&argv("ls")), "ls");
    }

    #[test]
    fn a_path_argument_is_not_a_subcommand() {
        assert_eq!(signature(&argv("ls -la /tmp")), "ls");
        assert_eq!(signature(&argv("cat ./notes.md")), "cat");
        assert_eq!(signature(&argv("rm -rf build/")), "rm");
        assert_eq!(signature(&argv("python3 script.py")), "python3");
        assert_eq!(signature(&argv("echo $HOME")), "echo");
    }

    #[test]
    fn flags_before_the_subcommand_do_not_become_it() {
        assert_eq!(signature(&argv("git --no-pager push")), "git push");
        assert_eq!(signature(&argv("npm --silent run build")), "npm run");
    }

    #[test]
    fn variants_share_one_signature() {
        for c in ["git push", "git push origin main", "git push --force"] {
            assert_eq!(signature(&argv(c)), "git push");
        }
    }

    #[test]
    fn broadening_drops_the_subcommand() {
        assert_eq!(broaden("git push"), "git");
        assert_eq!(broaden("ls"), "ls");
    }

    #[test]
    fn a_verdict_belongs_to_its_mode() {
        let mut v = Verdicts::default();
        v.set("contributor", "cargo build", entry(Action::Allow));
        assert!(v.lookup("contributor", "cargo build").is_some());
        assert!(
            v.lookup("reader", "cargo build").is_none(),
            "contributor's allow must not leak into reader"
        );
    }

    #[test]
    fn an_all_modes_verdict_applies_everywhere() {
        let mut v = Verdicts::default();
        v.set(ALL_MODES, "ls", entry(Action::Allow));
        assert!(v.lookup("reader", "ls").is_some());
        assert!(v.lookup("contributor", "ls").is_some());
    }

    #[test]
    fn the_modes_own_verdict_wins_over_the_shared_one() {
        let mut v = Verdicts::default();
        v.set(ALL_MODES, "cargo build", entry(Action::Deny));
        v.set("contributor", "cargo build", entry(Action::Allow));
        assert_eq!(
            v.lookup("contributor", "cargo build").unwrap().action,
            Action::Allow
        );
        assert_eq!(v.lookup("reader", "cargo build").unwrap().action, Action::Deny);
    }

    #[test]
    fn a_program_wide_verdict_covers_subcommands() {
        let mut v = Verdicts::default();
        v.set("reader", "git", entry(Action::Allow));
        assert!(v.lookup("reader", "git status").is_some());
    }
}
