//! The Claude Code PreToolUse wire format.
//!
//! Claude Code writes the pending tool call to our stdin as JSON and reads a
//! decision from our stdout. Printing nothing leaves the call to Claude Code's
//! own permission flow (rules, then the auto-mode classifier).

use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct HookInput {
    #[serde(default)]
    pub tool_name: String,
    #[serde(default)]
    pub tool_input: ToolInput,
    /// The session's working directory, used to resolve a relative path such
    /// as `rules.toml` when deciding whether a command touches askfirst.
    #[serde(default)]
    pub cwd: String,
    /// Which session this call belongs to. A mode set with `askfirst mode`
    /// applies to one session, so the hook has to know which one it is in.
    #[serde(default)]
    pub session_id: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct ToolInput {
    /// Present for Bash and PowerShell calls.
    #[serde(default)]
    pub command: Option<String>,
    /// Present for Edit, Write and MultiEdit.
    #[serde(default)]
    pub file_path: Option<String>,
    /// Present for NotebookEdit.
    #[serde(default)]
    pub notebook_path: Option<String>,
}

impl ToolInput {
    /// The file this call would change, whichever tool it came from.
    pub fn path(&self) -> Option<&str> {
        self.file_path
            .as_deref()
            .or(self.notebook_path.as_deref())
    }
}

/// What we tell Claude Code to do with the call.
///
/// `Pass` is not a wire value: it means we omit `permissionDecision` entirely,
/// which hands the call back to Claude Code's own flow. That is deliberately
/// distinct from `Allow`, which skips the classifier for that call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Decision {
    Allow,
    Pass,
    Ask,
    Deny,
}

impl Decision {
    pub fn wire(self) -> Option<&'static str> {
        match self {
            Decision::Allow => Some("allow"),
            Decision::Ask => Some("ask"),
            Decision::Deny => Some("deny"),
            Decision::Pass => None,
        }
    }
}

#[derive(Debug, Serialize)]
struct Output {
    #[serde(rename = "hookSpecificOutput")]
    hook_specific_output: Specific,
}

#[derive(Debug, Serialize)]
struct Specific {
    #[serde(rename = "hookEventName")]
    hook_event_name: &'static str,
    #[serde(rename = "permissionDecision", skip_serializing_if = "Option::is_none")]
    permission_decision: Option<&'static str>,
    #[serde(
        rename = "permissionDecisionReason",
        skip_serializing_if = "Option::is_none"
    )]
    permission_decision_reason: Option<String>,
    #[serde(rename = "additionalContext", skip_serializing_if = "Option::is_none")]
    additional_context: Option<String>,
}

/// Render a decision as the JSON Claude Code expects.
///
/// `reason` reaches the user on allow and ask, and the model on deny.
/// `context` reaches the model on every decision except defer, which we never
/// emit. Sending both is what lets an `ask` teach the agent something, which
/// `permissionDecisionReason` alone cannot do.
pub fn render(decision: Decision, reason: Option<String>, context: Option<String>) -> Option<String> {
    let wire = decision.wire();
    if wire.is_none() && context.is_none() {
        return None; // stay silent: Claude Code's own flow decides
    }
    let out = Output {
        hook_specific_output: Specific {
            hook_event_name: "PreToolUse",
            permission_decision: wire,
            permission_decision_reason: if wire.is_some() { reason } else { None },
            additional_context: context,
        },
    };
    serde_json::to_string(&out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strictest_decision_sorts_highest() {
        let mut v = [Decision::Allow, Decision::Deny, Decision::Pass, Decision::Ask];
        v.sort();
        assert_eq!(*v.last().unwrap(), Decision::Deny);
        assert!(Decision::Ask > Decision::Pass);
        assert!(Decision::Pass > Decision::Allow);
    }

    #[test]
    fn pass_with_no_context_emits_nothing() {
        assert!(render(Decision::Pass, None, None).is_none());
    }

    #[test]
    fn ask_carries_reason_and_context() {
        let s = render(Decision::Ask, Some("r".into()), Some("c".into())).unwrap();
        assert!(s.contains(r#""permissionDecision":"ask""#));
        assert!(s.contains(r#""permissionDecisionReason":"r""#));
        assert!(s.contains(r#""additionalContext":"c""#));
    }
}
