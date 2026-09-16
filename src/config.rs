//! Hand-written rules and the modes they live in, read fresh on every call.
//!
//! A mode is a named posture: what you are letting the agent do right now.
//! `reader` investigates, `contributor` changes things. Every rule belongs to
//! a mode and applies only while that mode is active. There is no shared tier
//! above them: a boundary you want everywhere is written into each mode that
//! should have it, where you can read it off the mode you are in.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::hook::Decision;

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct Config {
    /// What to do with a signature no rule and no verdict covers, when the
    /// active mode does not say.
    #[serde(default)]
    pub unseen: Unseen,

    /// The mode a session starts in when nothing else selects one.
    #[serde(default)]
    pub default_mode: Option<String>,

    #[serde(default)]
    pub modes: BTreeMap<String, Mode>,

    /// How to reach the model that decides an `agent` rule and an
    /// `unseen = "agent"` command. Named `judge` because `agent` is an
    /// action.
    #[serde(default)]
    pub judge: JudgeConfig,
}

/// Rules grouped by what they do.
///
/// One group per action rather than a list of objects each carrying an
/// `action`, so the file answers "what runs, what stops, what is refused"
/// without being read line by line. `allow` and `pass` are bare patterns:
/// neither needs anything said about it. `ask` and `deny` map a pattern to
/// the sentence you see when a call stops and the model is given to explain
/// it. `agent` maps a pattern to an extra line for the judge, which may be
/// empty.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct RuleSet {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pass: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub ask: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub deny: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub agent: BTreeMap<String, String>,
}

impl RuleSet {
    /// Flatten the groups into rules. Order does not decide anything: when
    /// several match, the firmest wins, and `first_match` breaks a tie on the
    /// more specific pattern.
    fn extend(&self, out: &mut Vec<Rule>) {
        let text = |s: &String| {
            let t = s.trim();
            (!t.is_empty()).then(|| t.to_string())
        };
        for p in &self.allow {
            out.push(Rule::bare(p, Action::Allow));
        }
        for p in &self.pass {
            out.push(Rule::bare(p, Action::Pass));
        }
        for (p, extra) in &self.agent {
            out.push(Rule {
                pattern: p.clone(),
                action: Action::Agent,
                context: None,
                prompt: text(extra),
            });
        }
        for (p, why) in &self.ask {
            out.push(Rule {
                pattern: p.clone(),
                action: Action::Ask,
                context: text(why),
                prompt: None,
            });
        }
        for (p, why) in &self.deny {
            out.push(Rule {
                pattern: p.clone(),
                action: Action::Deny,
                context: text(why),
                prompt: None,
            });
        }
    }

}

/// The judge askfirst spawns for an `agent` rule and for `unseen = "agent"`.
/// The prompt goes in on stdin and one word comes back on stdout.
#[derive(Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct JudgeConfig {
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

impl JudgeConfig {
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

impl Default for JudgeConfig {
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
    /// Everything this mode decides, grouped by action.
    #[serde(flatten)]
    pub rules: RuleSet,
}

/// One pattern and what it does, built from a group in the file.
#[derive(Debug, Clone)]
pub struct Rule {
    /// Space-separated words matched against the command's argv. `*` matches
    /// one word, and a trailing `*` matches any remaining words.
    pub pattern: String,
    pub action: Action,
    /// Why, in your words. It is shown to you when the call stops and given
    /// to the model, which is why there is one string rather than two: a
    /// short reason and a longer context said the same thing twice.
    pub context: Option<String>,
    /// `agent` rules only: an extra line of policy for the judge, about this
    /// pattern alone.
    pub prompt: Option<String>,
}

impl Rule {
    fn bare(pattern: &str, action: Action) -> Self {
        Self { pattern: pattern.to_string(), action, context: None, prompt: None }
    }
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

/// The action groups a mode is written in.
const GROUPS: [&str; 5] = ["allow", "pass", "agent", "ask", "deny"];

impl Config {
    /// Every rule in force right now, which is the active mode's and nothing
    /// else. A mode you have not defined has no rules, so `unseen` decides
    /// everything in it.
    pub fn rules_for(&self, mode: &str) -> Vec<Rule> {
        let mut v = Vec::new();
        if let Some(m) = self.modes.get(mode) {
            m.rules.extend(&mut v);
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

    /// Does this mode already decide this signature?
    ///
    /// Asked of the review queue: a signature waits until every mode has an
    /// answer for it, and this is what an answer looks like. A rule that only
    /// matches a longer command, such as `git push --force *` against the
    /// signature `git push`, does not decide the signature.
    pub fn covers(&self, mode: &str, signature: &str) -> bool {
        let argv: Vec<String> = signature.split_whitespace().map(str::to_string).collect();
        if argv.is_empty() {
            return true;
        }
        self.rules_for(mode)
            .iter()
            .any(|r| crate::rules::matches(&r.pattern, &argv))
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
            "//rules": "allow and pass are lists of patterns; ask, deny and agent map a pattern to a sentence (the judge's extra prompt, for agent)",
            "description": "",
            "unseen": "ask",
            "allow": [],
            "ask": {},
            "deny": {}
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

/// Write decisions into the rules file, in the modes they apply to.
///
/// This is what `askfirst review` does with your answers. They go in the same
/// file, in the same groups, as the rules you write by hand: one place says
/// what runs, and what review adds is something you can read, edit and delete
/// like anything else in it.
///
/// Anything already in the file is left exactly as it is, comments included,
/// because this edits the parsed JSON value rather than rewriting the file
/// from a struct.
pub fn add_rules(decisions: &[Decided]) -> Result<(), String> {
    let mut v = read_value()?;
    apply_decisions(&mut v, decisions)?;
    write_value(&v)
}

/// The edit itself, separated from reading and writing the file so it can be
/// tested on a value.
fn apply_decisions(v: &mut serde_json::Value, decisions: &[Decided]) -> Result<(), String> {
    let obj = v.as_object_mut().ok_or("rules file is not a JSON object")?;
    let modes = obj
        .entry("modes")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .ok_or("\"modes\" is not a JSON object")?;
    for d in decisions {
        let mode = modes
            .entry(d.mode.clone())
            .or_insert_with(|| serde_json::json!({}))
            .as_object_mut()
            .ok_or_else(|| format!("mode '{}' is not a JSON object", d.mode))?;
        let group = d.action.label();
        match d.action {
            Action::Allow | Action::Pass => {
                let list = mode
                    .entry(group)
                    .or_insert_with(|| serde_json::json!([]))
                    .as_array_mut()
                    .ok_or_else(|| format!("'{group}' in mode '{}' is not a list", d.mode))?;
                let entry = serde_json::Value::String(d.pattern.clone());
                if !list.contains(&entry) {
                    list.push(entry);
                }
            }
            Action::Ask | Action::Deny | Action::Agent => {
                let map = mode
                    .entry(group)
                    .or_insert_with(|| serde_json::json!({}))
                    .as_object_mut()
                    .ok_or_else(|| format!("'{group}' in mode '{}' is not an object", d.mode))?;
                map.insert(
                    d.pattern.clone(),
                    serde_json::Value::String(d.note.clone().unwrap_or_default()),
                );
            }
        }
    }
    Ok(())
}

/// One answer from review: what to write, where.
pub struct Decided {
    pub mode: String,
    pub action: Action,
    /// Already a pattern, not a signature: `cargo test *`, not `cargo test`.
    pub pattern: String,
    /// The sentence an `ask` or `deny` carries, or the judge's line for an
    /// `agent` rule. Empty for `allow` and `pass`, which say nothing.
    pub note: Option<String>,
}

/// Load the rule file. A missing file is not an error; a malformed one is,
/// and the caller turns that into an `ask` rather than letting a typo
/// silently disable the gate.
pub fn load(path: &Path) -> Result<Config, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    if let Some(where_) = old_format(&value) {
        // Unknown keys are ignored, so a file askfirst no longer understands
        // would load as a config with fewer rules, or none, and say nothing.
        // Refuse instead, which the caller turns into an ask.
        return Err(format!(
            "{}: rules at {where_}. Every rule belongs to a mode now, under \
             modes.<name>, and is grouped by action there: `allow` and `pass` are lists \
             of patterns, `ask` and `deny` map a pattern to a sentence, `agent` maps a \
             pattern to an extra prompt for the judge. The judge's own settings are under \
             `judge`. Write a boundary you want everywhere into each mode that should \
             have it.",
            path.display()
        ));
    }
    serde_json::from_value(value).map_err(|e| format!("{}: {e}", path.display()))
}

/// Where a file still carries a shape askfirst has stopped reading.
fn old_format(v: &serde_json::Value) -> Option<String> {
    if v.get("rules").is_some_and(|r| r.is_array()) {
        return Some("the top level".into());
    }
    // Rules used to be writable outside any mode, applying to all of them.
    // They are ignored now, which would quietly drop a boundary, so say so.
    if GROUPS.iter().any(|g| v.get(g).is_some()) {
        return Some("the top level".into());
    }
    let modes = v.get("modes")?.as_object()?;
    for (name, m) in modes {
        if m.get("rules").is_some_and(|r| r.is_array()) {
            return Some(format!("mode '{name}'"));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Config {
        serde_json::from_str(
            r#"{
              "default_mode": "contributor",
              "unseen": "ask",
              "modes": {
                "reader": {
                  "description": "Investigate only.",
                  "unseen": "deny",
                  "allow": ["git status *"],
                  "ask": { "* push *": "Confirm the remote." }
                },
                "contributor": {
                  "unseen": "ask",
                  "allow": ["cargo test *"],
                  "agent": { "python3 *": "scripts here only read." }
                }
              }
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn a_mode_is_the_only_place_rules_come_from() {
        let c = sample();
        assert_eq!(c.rules_for("reader").len(), 2);
        assert_eq!(c.rules_for("contributor").len(), 2);
        // Nothing sits above the modes, so a mode you have not defined has no
        // rules at all and `unseen` decides everything in it.
        assert!(c.rules_for("nonexistent").is_empty());
    }

    #[test]
    fn each_group_becomes_its_action() {
        let c = sample();
        let asked = c
            .rules_for("reader")
            .into_iter()
            .find(|r| r.action == Action::Ask)
            .expect("the ask group");
        assert_eq!(asked.pattern, "* push *");
        assert_eq!(asked.context.as_deref(), Some("Confirm the remote."));

        let judged = c
            .rules_for("contributor")
            .into_iter()
            .find(|r| r.action == Action::Agent)
            .expect("the agent group");
        assert_eq!(judged.pattern, "python3 *");
        assert_eq!(judged.prompt.as_deref(), Some("scripts here only read."));

        let allowed = c
            .rules_for("contributor")
            .into_iter()
            .find(|r| r.action == Action::Allow)
            .expect("the allow group");
        assert_eq!(allowed.pattern, "cargo test *");
        assert!(allowed.context.is_none() && allowed.prompt.is_none());
    }

    #[test]
    fn a_mode_covers_what_one_of_its_rules_matches() {
        let c = sample();
        assert!(c.covers("reader", "git status"));
        assert!(c.covers("reader", "git push"));
        // Nothing in reader answers for cargo, and nothing at all answers in a
        // mode that does not exist.
        assert!(!c.covers("reader", "cargo test"));
        assert!(!c.covers("nonexistent", "git status"));
        // A rule that only matches a longer command does not decide the
        // signature itself.
        let c: Config = serde_json::from_str(
            r#"{"modes": {"m": {"deny": {"git push --force *": "no"}}}}"#,
        )
        .unwrap();
        assert!(!c.covers("m", "git push"));
    }

    #[test]
    fn a_decision_is_written_into_the_group_its_action_names() {
        let mut v: serde_json::Value = serde_json::from_str(
            r#"{"//": "keep me", "modes": {"reader": {"allow": ["ls *"]}}}"#,
        )
        .unwrap();
        let decisions = vec![
            Decided {
                mode: "reader".into(),
                action: Action::Allow,
                pattern: "rg *".into(),
                note: None,
            },
            Decided {
                mode: "reader".into(),
                action: Action::Deny,
                pattern: "curl *".into(),
                note: Some("reader does not fetch".into()),
            },
            Decided {
                mode: "contributor".into(),
                action: Action::Agent,
                pattern: "python3 *".into(),
                note: None,
            },
        ];
        apply_decisions(&mut v, &decisions).unwrap();

        // Comments and existing rules survive: this edits the value, it does
        // not rewrite the file from a struct.
        assert_eq!(v["//"], "keep me");
        assert_eq!(v["modes"]["reader"]["allow"][0], "ls *");
        assert_eq!(v["modes"]["reader"]["allow"][1], "rg *");
        assert_eq!(v["modes"]["reader"]["deny"]["curl *"], "reader does not fetch");
        // A mode that did not exist yet is created, and an agent rule with no
        // line for the judge is an empty string.
        assert_eq!(v["modes"]["contributor"]["agent"]["python3 *"], "");

        // The same decision twice does not duplicate the pattern.
        apply_decisions(&mut v, &decisions).unwrap();
        assert_eq!(v["modes"]["reader"]["allow"].as_array().unwrap().len(), 2);

        // And the result is still a config askfirst can read.
        let c: Config = serde_json::from_value(v).unwrap();
        assert!(c.covers("reader", "rg"));
        assert!(c.covers("contributor", "python3"));
    }

    #[test]
    fn a_shape_askfirst_no_longer_reads_is_refused_rather_than_ignored() {
        // Serde ignores unknown keys, so an unconverted file would otherwise
        // load as a config with no rules and no complaint.
        let dir = std::env::temp_dir().join("askfirst-test-old");
        std::fs::create_dir_all(&dir).unwrap();
        for body in [
            r#"{"rules": [{"match": "git push *", "action": "ask"}]}"#,
            r#"{"modes": {"reader": {"rules": [{"match": "ls *", "action": "allow"}]}}}"#,
            // Rules outside a mode, from when there was a shared tier.
            r#"{"deny": {"git push --force *": "no"}}"#,
            r#"{"allow": ["ls *"]}"#,
        ] {
            let p = dir.join("rules.json");
            std::fs::write(&p, body).unwrap();
            let e = load(&p).expect_err("should refuse the old shape");
            assert!(e.contains("belongs to a mode"), "{e}");
        }
        let _ = std::fs::remove_file(dir.join("rules.json"));
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
        assert_eq!(c.judge.model, "haiku");
    }

    #[test]
    fn missing_file_is_an_empty_config() {
        let c = load(Path::new("/nonexistent/askfirst/rules.json")).unwrap();
        assert!(c.modes.is_empty());
        assert!(c.rules_for("contributor").is_empty());
    }

    #[test]
    fn the_judge_block_is_not_mistaken_for_an_agent_rule() {
        // `agent` is an action group now, so the judge's own settings live
        // under `judge`.
        let c: Config = serde_json::from_str(
            r#"{"judge": {"model": "sonnet", "timeout_secs": 9},
                "modes": {"x": {"agent": {"npx *": ""}}}}"#,
        )
        .unwrap();
        assert_eq!(c.judge.model, "sonnet");
        assert_eq!(c.judge.timeout_secs, 9);
        assert_eq!(c.rules_for("x").len(), 1);
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
