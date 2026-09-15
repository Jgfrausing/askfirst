//! Hand-written rules and the modes they live in, read fresh on every call.
//!
//! A mode is a named posture: what you are letting the agent do right now.
//! `reader` investigates, `contributor` changes things. Rules written at the
//! top level apply in every mode and are the invariants you never want a mode
//! to loosen; rules inside a mode apply only while that mode is active.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::hook::Decision;

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct Config {
    /// Rules that apply in every mode. A mode can make a call stricter, never
    /// looser, so these are the boundaries that hold whatever you switch to.
    #[serde(default)]
    pub rules: Vec<Rule>,

    /// What to do with a signature no rule and no verdict covers, when the
    /// active mode does not say.
    #[serde(default)]
    pub unseen: Unseen,

    /// The mode a session starts in when nothing else selects one.
    #[serde(default)]
    pub default_mode: Option<String>,

    #[serde(default)]
    pub modes: BTreeMap<String, Mode>,

    /// How to reach the model that judges an unseen command when `unseen` is
    /// `"agent"`.
    #[serde(default)]
    pub agent: AgentConfig,
}

/// The judge askfirst spawns for `unseen = "agent"`. The prompt goes in on
/// stdin and one word comes back on stdout.
#[derive(Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct AgentConfig {
    /// Which model judges. Anything the `claude` CLI accepts for `--model`:
    /// an alias such as `haiku` or `sonnet`, or a full model id.
    pub model: String,
    /// The program to run. Only change this for a judge that is not the
    /// Claude CLI; `model` is ignored when `args` is set explicitly.
    pub command: String,
    /// Leave empty to have askfirst build the arguments around `model`.
    pub args: Vec<String>,
    /// Must be comfortably under the hook's own timeout in settings.json, or
    /// Claude Code kills the hook before the judge answers.
    pub timeout_secs: u64,
}

impl AgentConfig {
    /// The argument vector to spawn with: yours if you set any, otherwise one
    /// built around `model`.
    pub fn argv(&self) -> Vec<String> {
        if !self.args.is_empty() {
            return self.args.clone();
        }
        vec![
            "-p".into(),
            "--model".into(),
            self.model.clone(),
            // The judge must not run this machine's hooks, or it would call
            // askfirst, which would call a judge, and so on.
            "--settings".into(),
            r#"{"disableAllHooks":true}"#.into(),
            // Loading this machine's MCP servers took about 5 of the 9 seconds
            // a bare `claude -p` spent before answering. The judge needs none
            // of them, and this runs in front of a tool call.
            "--strict-mcp-config".into(),
            "--no-session-persistence".into(),
            // It only has to answer a question; give it nothing to run.
            "--disallowed-tools".into(),
            "Bash,Edit,Write,Read,Glob,Grep,WebFetch,WebSearch".into(),
        ]
    }
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            model: "haiku".into(),
            command: "claude".into(),
            args: Vec::new(),
            timeout_secs: 20,
        }
    }
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct Mode {
    /// One line shown by `askfirst modes`, so the list explains itself.
    #[serde(default)]
    pub description: Option<String>,
    /// Overrides the top-level `unseen` while this mode is active.
    #[serde(default)]
    pub unseen: Option<Unseen>,
    #[serde(default)]
    pub rules: Vec<Rule>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Rule {
    /// Space-separated words matched against the command's argv. `*` matches
    /// one word, and a trailing `*` matches any remaining words.
    #[serde(rename = "match")]
    pub pattern: String,
    pub action: Action,
    /// Shown to you on ask and deny. Reaches the model only on deny.
    #[serde(default)]
    pub reason: Option<String>,
    /// Reaches the model on every decision.
    #[serde(default)]
    pub context: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Allow,
    Ask,
    Deny,
    Pass,
    /// Hand this call to the judge, every time, and use what it answers.
    ///
    /// For commands whose signature cannot tell you enough: `python3` says
    /// nothing about what the script does, and `docker run` nothing about the
    /// image. The judge sees the whole command and answers allow, ask or deny
    /// against the mode's policy.
    Agent,
}

impl Action {
    /// How firm this action is when several rules match one command.
    ///
    /// `agent` sits below `ask` because it may come back `allow`: if you
    /// wrote an explicit `ask` for something, that is firmer than asking a
    /// model to decide, and it wins.
    pub fn rank(self) -> u8 {
        match self {
            Action::Allow => 0,
            Action::Pass => 1,
            Action::Agent => 2,
            Action::Ask => 3,
            Action::Deny => 4,
        }
    }
}

impl Action {
    pub fn label(self) -> &'static str {
        match self {
            Action::Allow => "allow",
            Action::Ask => "ask",
            Action::Deny => "deny",
            Action::Pass => "pass",
            Action::Agent => "agent",
        }
    }
}

impl From<Action> for Decision {
    fn from(a: Action) -> Self {
        match a {
            Action::Allow => Decision::Allow,
            Action::Ask => Decision::Ask,
            Action::Deny => Decision::Deny,
            Action::Pass => Decision::Pass,
            // Only reached if nobody supplied a judge; the safe reading.
            Action::Agent => Decision::Ask,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Unseen {
    #[default]
    Ask,
    Deny,
    Pass,
    /// Hand the command to the model described by `[agent]`, judged against
    /// the mode's section of `policy.md`. Anything unclear comes back `ask`.
    Agent,
}

impl From<Unseen> for Decision {
    fn from(u: Unseen) -> Self {
        match u {
            Unseen::Ask => Decision::Ask,
            Unseen::Deny => Decision::Deny,
            Unseen::Pass => Decision::Pass,
            // Only reached if nobody supplied a judge; the safe reading.
            Unseen::Agent => Decision::Ask,
        }
    }
}

/// The one mode whose meaning is fixed in the binary rather than the config.
///
/// It allows everything, overriding every rule, verdict and `unseen` setting,
/// so it is always available as a way out even from a config that has locked
/// itself up. It cannot be redefined by editing the rules file: a `godmode`
/// entry there supplies only its description.
pub const GODMODE: &str = "godmode";

impl Config {
    /// Every rule in force right now: the shared ones plus the active mode's.
    pub fn rules_for<'a>(&'a self, mode: &str) -> Vec<&'a Rule> {
        let mut v: Vec<&Rule> = self.rules.iter().collect();
        if let Some(m) = self.modes.get(mode) {
            v.extend(m.rules.iter());
        }
        v
    }

    pub fn unseen_for(&self, mode: &str) -> Unseen {
        self.modes
            .get(mode)
            .and_then(|m| m.unseen)
            .unwrap_or(self.unseen)
    }

    /// godmode always exists, whatever the config says, so that a rules file
    /// that denies everything can still be escaped.
    pub fn knows_mode(&self, mode: &str) -> bool {
        mode == GODMODE || self.modes.contains_key(mode)
    }
}

/// The one directory askfirst uses. Rules, verdicts, the review queue and the
/// current mode all live here, so there is a single thing to protect, to back
/// up and to put under version control.
///
/// `pending.jsonl` is the odd one out: it is state rather than configuration,
/// and XDG would put it under `~/.local/state`. Keeping it here is the price
/// of having one directory to guard, which for this tool is worth more.
/// Where the config lives when nothing redirects it.
///
/// Guarded alongside the active directory, so pointing `ASKFIRST_HOME`
/// somewhere else adds a directory to protect rather than unprotecting this
/// one. Without that, setting the variable in the hook's environment moved the
/// fence off the real files.
pub fn default_dir() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config")
        });
    base.join("askfirst")
}

pub fn dir() -> PathBuf {
    if let Ok(p) = std::env::var("ASKFIRST_HOME") {
        return PathBuf::from(p);
    }
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config")
        });
    base.join("askfirst")
}

pub fn default_path() -> PathBuf {
    dir().join("rules.json")
}

/// Read the rules file as a JSON value, so a write can change one key and
/// leave everything else, including any `"//"` comment keys, untouched.
fn read_value() -> Result<serde_json::Value, String> {
    let path = default_path();
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

fn write_value(v: &serde_json::Value) -> Result<(), String> {
    let path = default_path();
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
    }
    let body = serde_json::to_string_pretty(v).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, format!("{body}\n")).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

/// Set the mode new sessions start in.
pub fn set_default_mode(mode: &str) -> Result<(), String> {
    let mut v = read_value()?;
    let obj = v.as_object_mut().ok_or("rules file is not a JSON object")?;
    obj.insert("default_mode".into(), serde_json::Value::String(mode.into()));
    write_value(&v)
}

/// Add a new mode to the rules file, and a matching entry to the policy.
pub fn create_mode(name: &str) -> Result<(), String> {
    let mut v = read_value().unwrap_or_else(|_| serde_json::json!({}));
    let obj = v.as_object_mut().ok_or("rules file is not a JSON object")?;
    let modes = obj
        .entry("modes")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .ok_or("\"modes\" is not a JSON object")?;
    modes.insert(
        name.into(),
        serde_json::json!({
            "//": "unseen: ask, deny, pass, or agent (judged against this mode's policy entry)",
            "description": "",
            "unseen": "ask",
            "rules": []
        }),
    );
    write_value(&v)?;

    // A mode with no policy entry judges nothing, so leave a stub for it.
    let ppath = crate::agent::policy_path();
    if let Ok(mut ptext) = std::fs::read_to_string(&ppath) {
        let key = format!("{name} = ");
        if !ptext.contains(&key) {
            if !ptext.ends_with('\n') {
                ptext.push('\n');
            }
            ptext.push_str(&format!(
                "\n{name} = \"\"\"\nDescribe what an agent may do in {name} mode.\n\nAllow: ...\n\nAsk: ...\n\nDeny: ...\n\"\"\"\n"
            ));
            let ptmp = ppath.with_extension("toml.tmp");
            std::fs::write(&ptmp, ptext).map_err(|e| e.to_string())?;
            std::fs::rename(&ptmp, &ppath).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// Load the rule file./// Load the rule file. A missing file is not an error; a malformed one is,
/// and the caller turns that into an `ask` rather than letting a typo
/// silently disable the gate.
pub fn load(path: &Path) -> Result<Config, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Config {
        serde_json::from_str(
            r#"{
              "default_mode": "contributor",
              "unseen": "ask",
              "rules": [{ "match": "* push *", "action": "ask" }],
              "modes": {
                "reader": {
                  "description": "Investigate only.",
                  "unseen": "deny",
                  "rules": [{ "match": "git status *", "action": "allow" }]
                },
                "contributor": {
                  "unseen": "ask",
                  "rules": [{ "match": "cargo test *", "action": "allow" }]
                }
              }
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn a_mode_adds_its_rules_to_the_shared_ones() {
        let c = sample();
        assert_eq!(c.rules_for("reader").len(), 2);
        assert_eq!(c.rules_for("contributor").len(), 2);
        // An unknown mode still gets the shared rules.
        assert_eq!(c.rules_for("nonexistent").len(), 1);
    }

    #[test]
    fn a_mode_can_override_unseen() {
        let c = sample();
        assert_eq!(c.unseen_for("reader"), Unseen::Deny);
        assert_eq!(c.unseen_for("contributor"), Unseen::Ask);
        assert_eq!(c.unseen_for("nonexistent"), Unseen::Ask);
    }

    #[test]
    fn modes_are_discoverable() {
        let c = sample();
        assert!(c.knows_mode("reader"));
        assert!(!c.knows_mode("admin"));
        assert_eq!(c.default_mode.as_deref(), Some("contributor"));
        assert_eq!(
            c.modes["reader"].description.as_deref(),
            Some("Investigate only.")
        );
    }

    #[test]
    fn unseen_defaults_to_ask() {
        let c: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(c.unseen, Unseen::Ask);
        assert!(c.modes.is_empty());
    }

    #[test]
    fn missing_file_is_an_empty_config() {
        let c = load(Path::new("/nonexistent/askfirst/rules.json")).unwrap();
        assert!(c.rules.is_empty());
    }

    #[test]
    fn malformed_file_is_an_error() {
        let dir = std::env::temp_dir().join("askfirst-test-bad");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("rules.json");
        std::fs::write(&p, "this is not json {{{").unwrap();
        assert!(load(&p).is_err());
        let _ = std::fs::remove_file(&p);
    }
}
