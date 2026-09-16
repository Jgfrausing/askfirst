//! What askfirst has seen and has no answer for yet.
//!
//! The queue, the current mode and the sessions it belongs to. What you decide
//! is not here: `askfirst review` writes it into the rules file, in the mode it
//! applies to, so there is one place that says what runs.
//!
//! Only `askfirst review` writes a rule. The hook appends observations and
//! nothing else, so the agent cannot widen its own permissions by running a
//! command: running it is exactly what puts it in the queue.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;

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

/// The program-only form of a signature, for when one rule should cover every
/// subcommand.
pub fn broaden(sig: &str) -> String {
    sig.split_whitespace().next().unwrap_or(sig).to_string()
}

/// One sighting of a signature no rule covers yet.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sighting {
    pub signature: String,
    pub example: String,

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

/// Append a sighting. Best effort: a hook that cannot write its queue still
/// returns its decision rather than failing the tool call.
pub fn record_sighting(signature: &str, example: &str) {
    let path = pending_path();
    if let Some(d) = path.parent() {
        if std::fs::create_dir_all(d).is_err() {
            return;
        }
    }
    let line = match serde_json::to_string(&Sighting {
        signature: signature.to_string(),
        example: example.chars().take(400).collect(),
    }) {
        Ok(l) => l,
        Err(_) => return,
    };
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{line}");
    }
}

/// Signatures seen but not yet decided, most-seen first.
///
/// One queue, whatever mode each sighting came up in. A signature waits until
/// every mode has a rule for it, so reviewing it once in front of you is the
/// shorter path than asking you the same question per mode.
pub fn pending(cfg: &crate::config::Config) -> Vec<(String, String, usize)> {
    let mut counts: BTreeMap<String, (String, usize)> = BTreeMap::new();
    for s in read_pending() {
        if decided(cfg, &s.signature) {
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

/// Is this signature settled? Only when every mode you have written decides
/// it, since a rule in one mode says nothing about the others. godmode is
/// skipped: it allows everything and reads no rules, so it would settle
/// everything on its own.
fn decided(cfg: &crate::config::Config, signature: &str) -> bool {
    let mut modes = cfg
        .modes
        .keys()
        .filter(|m| *m != crate::config::GODMODE)
        .peekable();
    modes.peek().is_some() && modes.all(|m| cfg.covers(m, signature))
}

/// Drop sightings that every mode now has a rule for.
pub fn prune_pending(cfg: &crate::config::Config) -> Result<(), String> {
    let path = pending_path();
    if !path.exists() {
        return Ok(());
    }
    let kept: Vec<Sighting> = read_pending()
        .into_iter()
        .filter(|s| !decided(cfg, &s.signature))
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
    fn a_sighting_from_when_the_queue_was_per_mode_still_reads() {
        // pending.jsonl is appended to, never rewritten in place, so lines
        // carrying the old `mode` field outlive the change.
        let s: Sighting =
            serde_json::from_str(r#"{"signature":"ls","example":"ls -la","mode":"reader"}"#)
                .unwrap();
        assert_eq!(s.signature, "ls");
        assert_eq!(s.example, "ls -la");
    }

}
