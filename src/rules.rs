//! Decide one tool call.
//!
//! Order per sub-command: a rule in the active mode, then unseen. What you
//! decided during review is a rule like any other, in the mode you decided it
//! for, so there is one place to look. Strictest wins across every
//! sub-command, so one asked segment makes the whole compound command ask.

use crate::config::{Config, Rule};
use crate::hook::Decision;
use crate::ledger;

/// Global flags that sit between a program and its subcommand. Skipping them
/// is what makes one `git push` signature cover `git -C . push` and
/// `git -c push.default=current push`, two spellings the docs list as misses
/// for a plain permission rule.
const SUBCOMMAND_SKIP: &[(&str, &[&str])] = &[
    ("git", &["-C", "-c", "--git-dir", "--work-tree", "--namespace", "--exec-path"]),
    ("docker", &["-H", "--context", "--config"]),
    ("kubectl", &["-n", "--namespace", "--context", "--kubeconfig"]),
];

/// Wrappers that take a command as their argument. `sudo git push` should
/// carry the `git push` signature, not a `sudo` one.
const WRAPPERS: &[&str] =
    &["sudo", "doas", "env", "nohup", "time", "timeout", "nice", "xargs", "command"];

/// What askfirst asks the judge: a command, and the rule's own prompt when it
/// carries one. It answers a decision and a line about why.
pub type Judge<'a> = &'a mut dyn FnMut(&str, Option<&str>) -> (Decision, Option<String>);

pub struct Verdict {
    pub decision: Decision,
    pub reason: Option<String>,
    pub context: Option<String>,
    /// Signatures with no rule and no verdict, for the review queue.
    pub unseen: Vec<(String, String)>,
}

/// Decide a call. `judge` is consulted for an `agent` rule and for a
/// signature with no rule and no verdict when the mode's `unseen` is `agent`.
/// Its second argument is the rule's own extra prompt, if it carries one. It
/// is injected so the decision logic stays testable without spawning
/// anything.
pub fn evaluate(
    cfg: &Config,
    parsed: &crate::parse::Parsed,
    cwd: &str,
    mode: &str,
    judge: Judge,
) -> Verdict {
    let commands = &parsed.commands;
    let mut best: Option<(Decision, Option<String>, Option<String>)> = None;
    let mut unseen: Vec<(String, String)> = Vec::new();

    // A command that names askfirst's own files is floored at ask, whatever
    // the rules and verdicts say. Otherwise the first thing an agent learns
    // is that the gate can be edited.
    let protected = crate::selfguard::protected_paths();
    let mut named: Vec<String> = parsed.redirects.clone();
    for c in commands {
        named.extend(c.argv.iter().cloned());
    }
    let path_touch = crate::selfguard::touches_self(&named, cwd, &protected);

    // godmode is decided here, not by the rules, so no rules file can take it
    // away and none can water it down. The self-guard below still applies: it
    // is what keeps godmode reversible, since a session that could rewrite the
    // rules file could make godmode permanent from a single approval.
    if mode == crate::config::GODMODE {
        // A call to askfirst itself carries no path, so the path check misses
        // it. Normalised, so `sudo askfirst review` and `/usr/bin/askfirst
        // review` are caught as readily as the bare spelling. Every other mode
        // handles this per sub-command, in the loop below.
        let self_invoke = commands
            .iter()
            .find_map(|c| crate::selfguard::invokes_self(&normalise(&c.argv)));
        let (mut decision, mut reason, mut context) = (Decision::Allow, None, None);
        if let Some(word) = path_touch.clone().or(self_invoke) {
            decision = Decision::Ask;
            reason = Some(format!(
                "godmode allows everything except askfirst's own configuration ('{word}')"
            ));
            context = Some(format!(
                "askfirst is in godmode, which allows every command. It still asks before its own configuration or binary is touched ('{word}'), because changing those would outlast this session. Wait for the user."
            ));
        }
        return Verdict { decision, reason, context, unseen: Vec::new() };
    }

    let in_force = cfg.rules_for(mode);

    let mut consider = |d: Decision, reason: Option<String>, context: Option<String>| {
        if best.as_ref().is_none_or(|(bd, _, _)| d > *bd) {
            best = Some((d, reason, context));
        }
    };

    for cmd in commands {
        let argv = normalise(&cmd.argv);
        if argv.is_empty() {
            continue;
        }
        let sig = ledger::signature(&argv);
        let example = cmd.argv.join(" ");

        // Running askfirst is decided by a rule or a verdict that names it,
        // and asks when neither does. The `unseen` step below says why.
        let self_call = crate::selfguard::invokes_self(&argv).is_some();

        // 1. A rule in this mode wins outright. A rule about
        //    askfirst has to name askfirst, so that a catch-all written about
        //    other commands cannot open the gate.
        let rule = if self_call {
            first_match(
                in_force.iter().filter(|r| crate::selfguard::pattern_names_self(&r.pattern)),
                &argv,
            )
        } else {
            first_match(&in_force, &argv)
        };
        if let Some(rule) = rule {
            if rule.action == crate::config::Action::Agent {
                let (d, why) = judge(&example, rule.prompt.as_deref());
                let why = why.unwrap_or_else(|| "judged against your policy".into());
                consider(
                    d,
                    Some(format!("'{sig}' is always judged: {why}")),
                    Some(judged_context(&sig, mode, &why, rule.prompt.as_deref())),
                );
            } else {
                // One string does both jobs: it is what you are shown when the
                // call stops and what the model is told about it.
                consider(rule.action.into(), rule.context.clone(), rule.context.clone());
            }
            continue;
        }

        // 2. Never seen. askfirst's own commands do not take the mode's
        //    answer for an uncategorised command: they ask. `askfirst mode`
        //    is the only way out of a mode, so a mode whose `unseen` is deny
        //    would swallow it with no prompt to approve (reader did exactly
        //    that), and one whose `unseen` is pass or allow would let a
        //    session change the gate without you seeing it. They are not
        //    queued either: a verdict earned by running askfirst would be
        //    self-granting.
        if self_call {
            consider(
                Decision::Ask,
                Some(format!("'{sig}' runs askfirst itself")),
                Some(
                    "This command runs askfirst itself, which asks unless the user has written a rule naming that command: \
                     `askfirst review` records permissions and `askfirst mode` changes what runs without asking. \
                     `askfirst check \"<command>\"` is the exception, reporting what would happen and changing nothing, \
                     so use that if you want to know where a boundary is. \
                     Wait for the user; do not rewrite the invocation to avoid this check."
                        .to_string(),
                ),
            );
            continue;
        }

        // Anything else is queued either way, then handled now: the mode's
        // fixed answer, or the judge.
        unseen.push((sig.clone(), example.clone()));
        if cfg.unseen_for(mode) == crate::config::Unseen::Agent {
            let (d, why) = judge(&example, None);
            let why = why.unwrap_or_else(|| "judged against your policy".into());
            consider(
                d,
                Some(format!("'{sig}' is new: {why}")),
                Some(format!(
                    "askfirst has not seen '{sig}' before, so it judged this call against the '{mode}' section of your policy.md: {why}. \
                     This decision applies to this call only and is not recorded. The user makes it permanent with `askfirst review`. \
                     Do not rephrase the command to get a different signature."
                )),
            );
        } else {
            consider(
                cfg.unseen_for(mode).into(),
                Some(format!("'{sig}' has not been categorised yet")),
                Some(format!(
                    "askfirst has not seen '{sig}' before, so it has no decision for it and is asking the user. \
                     The user categorises it with `askfirst review`. Wait for their answer; do not rephrase the command to get a different signature."
                )),
            );
        }
    }

    let (mut decision, mut reason, mut context) = best.unwrap_or((Decision::Pass, None, None));

    if let Some(word) = path_touch {
        // An explicit deny on askfirst's files is a boundary worth keeping,
        // so this only raises a softer decision to a prompt.
        decision = decision.max(Decision::Ask);
        // The explanation is the self-guard's, even when something else set
        // the decision: being told the call was uncategorised, when it was
        // actually stopped for touching the gate, teaches the agent the wrong
        // lesson and invites a retry under another name.
        reason = Some(format!(
            "this command names askfirst's own configuration ('{word}')"
        ));
        context = Some(format!(
            "This command names askfirst's own configuration or binary ('{word}'). askfirst always asks before its own rules, verdicts or executable are touched, so that the gate cannot be widened without the user seeing it. Wait for the user; do not rewrite the path to avoid this check."
        ));
        // Never queue a signature learned from a call that edits the gate: a
        // verdict earned this way would be self-granting.
        unseen.clear();
    }

    Verdict { decision, reason, context, unseen }
}

/// What the model is told when a rule or verdict routed this call to the
/// judge. `extra` is the rule's own prompt, which the judge saw too.
fn judged_context(sig: &str, mode: &str, why: &str, extra: Option<&str>) -> String {
    let mut s = format!(
        "'{sig}' is always judged rather than decided by its name alone, because the name does not say what it will do.          askfirst judged this call against the '{mode}' policy: {why}.          The verdict applies to this call only and is not recorded.          Do not rephrase the command to get a different signature."
    );
    if let Some(e) = extra {
        s.push(' ');
        s.push_str(e);
    }
    s
}

/// The rule that decides a command: the firmest that matches, and among
/// equally firm ones the most specific, so `git push --force *` explains
/// itself rather than whichever equally firm rule was read first.
fn first_match<'a>(
    rules: impl IntoIterator<Item = &'a Rule>,
    argv: &[String],
) -> Option<&'a Rule> {
    let mut chosen: Option<&Rule> = None;
    for rule in rules {
        if !matches(&rule.pattern, argv) {
            continue;
        }
        let better = chosen.is_none_or(|c| {
            (rule.action.rank(), rule.pattern.len()) > (c.action.rank(), c.pattern.len())
        });
        if better {
            chosen = Some(rule);
        }
    }
    chosen
}

/// Strip wrappers, resolve the program to its base name, and drop global
/// flags, so spellings of the same operation share a signature.
pub fn normalise(argv: &[String]) -> Vec<String> {
    let mut v: Vec<String> = argv.to_vec();
    for _ in 0..8 {
        let Some(first) = v.first().map(|s| base_name(s)) else { break };
        if !WRAPPERS.contains(&first.as_str()) {
            break;
        }
        let rest: Vec<String> = v
            .iter()
            .skip(1)
            .skip_while(|a| a.starts_with('-') || a.contains('='))
            .cloned()
            .collect();
        if rest.is_empty() {
            break;
        }
        v = rest;
    }
    if v.is_empty() {
        return v;
    }
    v[0] = base_name(&v[0]);

    let prog = v[0].clone();
    let Some((_, flags)) = SUBCOMMAND_SKIP.iter().find(|(p, _)| *p == prog) else {
        return v;
    };
    let mut out = vec![prog];
    let mut i = 1;
    while i < v.len() {
        let a = &v[i];
        if let Some(eq) = a.find('=') {
            if flags.contains(&&a[..eq]) {
                i += 1;
                continue;
            }
        }
        if flags.contains(&a.as_str()) {
            i += 2; // flag and its value
            continue;
        }
        out.push(a.clone());
        i += 1;
    }
    out
}

fn base_name(s: &str) -> String {
    std::path::Path::new(s)
        .file_name()
        .and_then(|o| o.to_str())
        .unwrap_or(s)
        .to_string()
}

/// `*` matches exactly one word. A trailing `*` matches any remaining words,
/// including none, so `git push *` covers a bare `git push`, and `* push *`
/// covers a push by whatever program.
pub fn matches(pattern: &str, argv: &[String]) -> bool {
    let pat: Vec<&str> = pattern.split_whitespace().collect();
    if pat.is_empty() {
        return false;
    }
    let trailing = *pat.last().unwrap() == "*";
    let fixed = if trailing { &pat[..pat.len() - 1] } else { &pat[..] };

    if argv.len() < fixed.len() {
        return false;
    }
    if !trailing && argv.len() != fixed.len() {
        return false;
    }
    fixed
        .iter()
        .zip(argv)
        .all(|(p, a)| *p == "*" || p.eq_ignore_ascii_case(a))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Action, Unseen};

    /// A config whose one mode, the one the tests run in, holds these
    /// patterns in the group each action names. `ask`, `deny` and `agent`
    /// carry a sentence, as they do in a real file.
    fn cfg(rules: Vec<(&str, Action)>, unseen: Unseen) -> Config {
        let mut set = crate::config::RuleSet::default();
        for (pattern, action) in rules {
            let p = pattern.to_string();
            match action {
                Action::Allow => set.allow.push(p),
                Action::Pass => set.pass.push(p),
                Action::Ask => {
                    set.ask.insert(p, "c".into());
                }
                Action::Deny => {
                    set.deny.insert(p, "c".into());
                }
                Action::Agent => {
                    set.agent.insert(p, String::new());
                }
            }
        }
        let mut modes = std::collections::BTreeMap::new();
        modes.insert(
            MODE.to_string(),
            crate::config::Mode { description: None, unseen: Some(unseen), rules: set },
        );
        Config { default_mode: None, modes, judge: Default::default(), unseen }
    }

    const MODE: &str = "contributor";

    fn run(c: &Config, cmd: &str) -> Verdict {
        run_judged(c, cmd, &mut |_, _| (Decision::Ask, None))
    }

    fn run_judged(c: &Config, cmd: &str, judge: Judge) -> Verdict {
        evaluate(
            c,
            &crate::parse::parse(cmd),
            "/tmp/askfirst-not-the-config-dir",
            MODE,
            judge,
        )
    }

    #[test]
    fn an_unseen_command_asks_and_is_queued() {
        let r = run(&cfg(vec![], Unseen::Ask), "mystery-tool --go");
        assert_eq!(r.decision, Decision::Ask);
        assert_eq!(r.unseen.len(), 1);
        assert_eq!(r.unseen[0].0, "mystery-tool");
    }

    #[test]
    fn a_decided_signature_is_applied_and_not_requeued() {
        // What review writes is a rule like any other: the pattern it puts in
        // the file is the signature plus a trailing `*`.
        let c = cfg(vec![("cargo test *", Action::Allow)], Unseen::Ask);
        let r = run(&c, "cargo test --release");
        assert_eq!(r.decision, Decision::Allow);
        assert!(r.unseen.is_empty());
    }

    #[test]
    fn a_broadened_decision_covers_every_subcommand() {
        // `b` during review writes the program alone.
        let c = cfg(vec![("ls *", Action::Allow)], Unseen::Ask);
        assert_eq!(run(&c, "ls -la /tmp").decision, Decision::Allow);
        let c = cfg(vec![("git *", Action::Allow)], Unseen::Ask);
        assert_eq!(run(&c, "git status --short").decision, Decision::Allow);
    }

    #[test]
    fn unknown_aliases_ask_without_any_discovery() {
        // The point of the ledger: askfirst does not need to know that `dot`
        // and `git publish` push. It has not seen them, so they stop.
        let c = cfg(vec![("git status *", Action::Allow)], Unseen::Ask);
        for cmd in ["dot push", "git publish", "gj push", "hg push"] {
            let r = run(&c, cmd);
            assert_eq!(r.decision, Decision::Ask, "{cmd} did not stop");
            assert_eq!(r.unseen.len(), 1, "{cmd} was not queued");
        }
    }

    #[test]
    fn spellings_of_one_operation_share_a_signature() {
        let c = cfg(vec![], Unseen::Ask);
        for cmd in [
            "git push origin main",
            "git -C . push",
            "git -c push.default=current push",
            "/usr/bin/git push",
            "sudo git push",
        ] {
            assert_eq!(run(&c, cmd).unseen[0].0, "git push", "{cmd}");
        }
    }

    #[test]
    fn running_askfirst_asks_whatever_the_mode_would_do_with_it() {
        // Not the mode's `unseen`: pass would let a session change the gate
        // unseen, deny would swallow the command that leaves the mode.
        for unseen in [Unseen::Pass, Unseen::Deny, Unseen::Ask] {
            let c = cfg(vec![], unseen);
            for cmd in ["askfirst review", "askfirst mode contributor", "askfirst mode godmode"] {
                assert_eq!(run(&c, cmd).decision, Decision::Ask, "did not stop: {cmd}");
            }
        }
        // And no spelling slips past it. These carry a second sub-command, so
        // they are checked where that one decides nothing on its own.
        let c = cfg(vec![], Unseen::Pass);
        for cmd in [
            "printf 'a\\n' | askfirst review",
            "ASKFIRST_HOME=/tmp/x askfirst review",
            "cd /tmp && askfirst review",
        ] {
            assert_eq!(run(&c, cmd).decision, Decision::Ask, "did not stop: {cmd}");
        }
    }

    #[test]
    fn a_strict_neighbour_still_decides_the_whole_line() {
        // Strictest wins across sub-commands, as everywhere else: pairing a
        // mode switch with an uncategorised command in a mode that denies
        // those refuses the line. The switch on its own is still approvable.
        let c = cfg(vec![], Unseen::Deny);
        assert_eq!(run(&c, "cd /tmp && askfirst mode contributor").decision, Decision::Deny);
        assert_eq!(run(&c, "askfirst mode contributor").decision, Decision::Ask);
    }

    #[test]
    fn a_rule_naming_askfirst_decides_it() {
        // The ask is a default, not a floor. Naming the command in the rules
        // file is deliberate, and the rules file is itself guarded.
        let c = cfg(vec![("askfirst mode *", Action::Allow)], Unseen::Ask);
        assert_eq!(run(&c, "askfirst mode reader").decision, Decision::Allow);
        // Only what the rule names: review has no rule, so it still asks.
        assert_eq!(run(&c, "askfirst review").decision, Decision::Ask);

        let c = cfg(vec![("askfirst review *", Action::Deny)], Unseen::Ask);
        assert_eq!(run(&c, "askfirst review").decision, Decision::Deny);
    }

    #[test]
    fn a_catch_all_rule_does_not_decide_askfirst() {
        // A rule written about other commands sweeps askfirst up without
        // naming it. Letting it through would open the gate by accident.
        for pattern in ["*", "* mode *"] {
            let c = cfg(vec![(pattern, Action::Allow)], Unseen::Pass);
            assert_eq!(
                run(&c, "askfirst mode godmode").decision,
                Decision::Ask,
                "'{pattern}' should not decide askfirst"
            );
        }
    }

    #[test]
    fn only_check_escapes_the_self_guard() {
        let c = cfg(vec![], Unseen::Pass);
        for cmd in ["askfirst review", "askfirst mode reader", "askfirst modes", "askfirst"] {
            assert_eq!(run(&c, cmd).decision, Decision::Ask, "should ask: {cmd}");
        }
        assert_eq!(
            run(&c, "askfirst check ls").decision,
            Decision::Pass,
            "check reports and changes nothing, so it does not ask"
        );
    }

    #[test]
    fn a_wrapper_cannot_hide_a_self_call() {
        let c = cfg(vec![], Unseen::Pass);
        for cmd in ["sudo askfirst review", "env askfirst mode reader", "/usr/bin/askfirst review"] {
            assert_eq!(run(&c, cmd).decision, Decision::Ask, "should ask: {cmd}");
        }
    }

    #[test]
    fn an_uncategorised_mode_switch_is_never_swallowed() {
        // The lock-out this fixes: reader sets unseen = deny, `askfirst mode`
        // has no rule and no verdict, so the deny swallowed the only way out
        // of reader and no prompt was ever offered.
        let c = cfg(vec![("git *", Action::Deny)], Unseen::Deny);
        for cmd in ["askfirst mode contributor", "askfirst mode godmode", "askfirst review"] {
            assert_eq!(
                run(&c, cmd).decision,
                Decision::Ask,
                "must stay approvable: {cmd}"
            );
        }
    }

    #[test]
    fn an_explicit_deny_on_askfirsts_files_still_denies() {
        // Only invoking askfirst is capped at ask. A deny rule reaching its
        // files is a boundary worth keeping.
        let c = cfg(vec![("sed *", Action::Deny)], Unseen::Pass);
        let r = run(
            &c,
            "sed -i s/a/b/ /tmp/askfirst-not-the-config-dir/rules.json",
        );
        assert_eq!(r.decision, Decision::Deny);
    }

    #[test]
    fn a_self_call_is_never_queued_for_review() {
        let r = run(&cfg(vec![], Unseen::Ask), "askfirst review");
        assert!(r.unseen.is_empty(), "a self call must not earn its own verdict");
    }

    #[test]
    fn an_agent_rule_hands_its_own_prompt_to_the_judge() {
        let mut c = cfg(vec![("python3 *", Action::Agent)], Unseen::Ask);
        c.modes.get_mut(MODE).unwrap().rules.agent.insert("python3 *".into(), "scripts in this repo only read.".into());
        let mut seen: Vec<(String, Option<String>)> = Vec::new();
        let mut judge = |cmd: &str, extra: Option<&str>| {
            seen.push((cmd.to_string(), extra.map(str::to_string)));
            (Decision::Allow, Some("policy says fine".into()))
        };
        let r = run_judged(&c, "python3 report.py", &mut judge);
        assert_eq!(r.decision, Decision::Allow);
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].1.as_deref(), Some("scripts in this repo only read."));
        // An unseen command judged by the mode carries no rule prompt.
        let mut plain = |_: &str, extra: Option<&str>| {
            assert!(extra.is_none(), "unseen judging has no rule to take a prompt from");
            (Decision::Allow, None)
        };
        let c = cfg(vec![], Unseen::Agent);
        run_judged(&c, "cargo build", &mut plain);
    }

    #[test]
    fn the_most_specific_of_two_equally_firm_rules_explains_the_stop() {
        // Both deny; the one that names the flag is the one worth quoting.
        let mut c = cfg(vec![], Unseen::Pass);
        c.modes.get_mut(MODE).unwrap().rules.deny.insert("git *".into(), "git is off here.".into());
        c.modes.get_mut(MODE).unwrap().rules.deny.insert("git push --force *".into(), "Force push rewrites history.".into());
        let r = run(&c, "git push --force origin main");
        assert_eq!(r.decision, Decision::Deny);
        assert_eq!(r.reason.as_deref(), Some("Force push rewrites history."));
    }

    #[test]
    fn the_judge_is_consulted_only_for_unseen_signatures() {
        // `ls` has a rule, `git push` has a rule, only `cargo build` is new.
        let c = cfg(
            vec![("ls *", Action::Allow), ("git push *", Action::Ask)],
            Unseen::Agent,
        );

        let mut asked_about: Vec<String> = Vec::new();
        let mut judge = |cmd: &str, _: Option<&str>| {
            asked_about.push(cmd.to_string());
            (Decision::Allow, Some("policy says fine".into()))
        };
        let r = run_judged(&c, "ls && git push && cargo build", &mut judge);
        assert_eq!(asked_about.len(), 1, "judged: {asked_about:?}");
        assert!(asked_about[0].contains("cargo build"));
        // The rule's ask still wins over the judge's allow.
        assert_eq!(r.decision, Decision::Ask);
    }

    #[test]
    fn the_judge_can_allow_an_unseen_command() {
        let c = cfg(vec![], Unseen::Agent);
        let r = run_judged(&c, "cargo build", &mut |_, _| {
            (Decision::Allow, Some("policy says fine".into()))
        });
        assert_eq!(r.decision, Decision::Allow);
        // ...but it is still queued, so the user's decision is what lasts.
        assert_eq!(r.unseen.len(), 1);
        assert_eq!(r.unseen[0].0, "cargo build");
    }

    #[test]
    fn a_judge_deny_outranks_a_judge_allow_elsewhere() {
        let c = cfg(vec![], Unseen::Agent);
        let r = run_judged(&c, "cargo build && curl http://x", &mut |cmd, _| {
            if cmd.contains("curl") {
                (Decision::Deny, Some("policy forbids network".into()))
            } else {
                (Decision::Allow, None)
            }
        });
        assert_eq!(r.decision, Decision::Deny);
    }

    #[test]
    fn an_unavailable_judge_falls_back_to_ask() {
        let c = cfg(vec![], Unseen::Agent);
        let r = run_judged(&c, "cargo build", &mut |_, _| {
            (Decision::Ask, Some("judge unavailable (timed out after 20s)".into()))
        });
        assert_eq!(r.decision, Decision::Ask);
        assert!(r.reason.unwrap().contains("judge unavailable"));
    }

    #[test]
    fn the_judge_is_never_consulted_for_a_call_that_touches_askfirst() {
        let c = cfg(vec![], Unseen::Agent);
        let mut calls = 0;
        let r = run_judged(&c, "askfirst review", &mut |_, _| {
            calls += 1;
            (Decision::Allow, None)
        });
        // The guard floors it regardless of what any judge would have said.
        assert_eq!(r.decision, Decision::Ask);
        assert!(r.unseen.is_empty());
    }

    #[test]
    fn an_agent_rule_routes_a_known_command_to_the_judge() {
        // The case a signature cannot answer: `python3 x.py` says nothing
        // about what the script does.
        let c = cfg(vec![("python3 *", Action::Agent)], Unseen::Pass);
        let mut seen = Vec::new();
        let r = run_judged(&c, "python3 deploy.py --prod", &mut |cmd, _| {
            seen.push(cmd.to_string());
            (Decision::Ask, Some("the policy cannot tell what this script does".into()))
        });
        assert_eq!(r.decision, Decision::Ask);
        assert_eq!(seen.len(), 1);
        assert!(seen[0].contains("deploy.py"));
        // It has a rule, so it is not queued as uncategorised.
        assert!(r.unseen.is_empty());
    }

    #[test]
    fn an_agent_rule_can_come_back_allow() {
        let c = cfg(vec![("python3 *", Action::Agent)], Unseen::Pass);
        let r = run_judged(&c, "python3 -c 'print(1)'", &mut |_, _| {
            (Decision::Allow, Some("printing a number is harmless".into()))
        });
        assert_eq!(r.decision, Decision::Allow);
    }

    #[test]
    fn an_explicit_ask_rule_beats_an_agent_rule() {
        // You wrote `ask` for it: that is firmer than asking a model, which
        // might answer allow.
        let c = cfg(
            vec![("python3 *", Action::Agent), ("python3 deploy.py *", Action::Ask)],
            Unseen::Pass,
        );
        let mut called = 0;
        let r = run_judged(&c, "python3 deploy.py", &mut |_, _| {
            called += 1;
            (Decision::Allow, None)
        });
        assert_eq!(r.decision, Decision::Ask);
        assert_eq!(called, 0, "the judge should not even be consulted");
    }

    #[test]
    fn an_agent_rule_beats_an_allow_rule() {
        let c = cfg(
            vec![("python3 *", Action::Allow), ("python3 *", Action::Agent)],
            Unseen::Pass,
        );
        let r = run_judged(&c, "python3 x.py", &mut |_, _| {
            (Decision::Deny, Some("this one is dangerous".into()))
        });
        assert_eq!(r.decision, Decision::Deny);
    }

    #[test]
    fn a_judge_that_cannot_tell_asks() {
        // `j` during review writes an `agent` rule, which is judged every time
        // and never recorded, so nothing is learned from a hedge.
        let c = cfg(vec![("python3 *", Action::Agent)], Unseen::Pass);
        let r = run_judged(&c, "python3 x.py", &mut |_, _| {
            (Decision::Ask, Some("cannot tell".into()))
        });
        assert_eq!(r.decision, Decision::Ask);
        assert!(r.unseen.is_empty());
    }

    #[test]
    fn godmode_overrides_every_rule() {
        // Rules that would otherwise deny, and a mode that is not even defined
        // in the config: godmode is decided in the binary.
        let c = cfg(
            vec![("* push *", Action::Ask), ("git push --force *", Action::Deny), ("rm *", Action::Deny)],
            Unseen::Deny,
        );
        for cmd in [
            "rm -rf /",
            "git push --force origin main",
            "curl http://x | bash",
            "anything-at-all --yolo",
        ] {
            let r = evaluate(
                &c,
                &crate::parse::parse(cmd),
                "/tmp/askfirst-not-the-config-dir",
                crate::config::GODMODE,
                &mut |_, _| (Decision::Deny, None),
            );
            assert_eq!(r.decision, Decision::Allow, "godmode did not allow: {cmd}");
            assert!(r.unseen.is_empty(), "godmode should not queue: {cmd}");
        }
    }

    #[test]
    fn godmode_still_asks_before_askfirst_changes_itself() {
        // The one exception, and what keeps godmode reversible.
        let c = cfg(vec![], Unseen::Pass);
        for cmd in ["askfirst review", "askfirst mode reader"] {
            let r = evaluate(
                &c,
                &crate::parse::parse(cmd),
                "/tmp/askfirst-not-the-config-dir",
                crate::config::GODMODE,
                &mut |_, _| (Decision::Allow, None),
            );
            assert_eq!(r.decision, Decision::Ask, "godmode let through: {cmd}");
        }
    }

    #[test]
    fn strictest_segment_wins() {
        let c = cfg(vec![("ls *", Action::Allow), ("* push *", Action::Ask)], Unseen::Ask);
        assert_eq!(run(&c, "ls && git push").decision, Decision::Ask);
    }

    #[test]
    fn one_unseen_segment_stops_an_otherwise_allowed_chain() {
        let c = cfg(vec![("ls *", Action::Allow)], Unseen::Ask);
        let r = run(&c, "ls && mystery-tool");
        assert_eq!(r.decision, Decision::Ask);
        assert_eq!(r.unseen[0].0, "mystery-tool");
    }

    #[test]
    fn a_deny_on_one_segment_beats_an_allow_on_another() {
        let c = cfg(vec![("ls *", Action::Allow), ("curl *", Action::Deny)], Unseen::Ask);
        assert_eq!(run(&c, "ls && curl http://x").decision, Decision::Deny);
    }

    #[test]
    fn everything_known_and_allowed_stays_quiet() {
        let c = cfg(vec![("ls *", Action::Allow), ("cargo build *", Action::Allow)], Unseen::Ask);
        let r = run(&c, "ls && cargo build");
        assert_eq!(r.decision, Decision::Allow);
        assert!(r.unseen.is_empty());
    }
}
