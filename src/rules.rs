//! Decide one tool call.
//!
//! Order per sub-command: a hand-written rule, then a verdict you gave during
//! review, then unseen. Strictest wins across every sub-command, so one
//! asked segment makes the whole compound command ask.

use crate::config::{Config, Rule};
use crate::hook::Decision;
use crate::ledger::{self, Verdicts};

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

pub struct Verdict {
    pub decision: Decision,
    pub reason: Option<String>,
    pub context: Option<String>,
    /// Signatures with no rule and no verdict, for the review queue.
    pub unseen: Vec<(String, String)>,
}

/// Decide a call. `judge` is consulted only for a signature with no rule and
/// no verdict when the mode's `unseen` is `agent`; it is injected so the
/// decision logic stays testable without spawning anything.
pub fn evaluate(
    cfg: &Config,
    verdicts: &Verdicts,
    parsed: &crate::parse::Parsed,
    cwd: &str,
    mode: &str,
    judge: &mut dyn FnMut(&str) -> (Decision, Option<String>),
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
    let mut self_touch = path_touch.clone();
    // A call to askfirst itself carries no path, so the check above misses it.
    // `askfirst review` is what writes verdicts: an agent that can run it can
    // approve its own queue.
    // Normalised, so `sudo askfirst review` and `/usr/bin/askfirst review`
    // are caught as readily as the bare spelling.
    let self_invoke = commands
        .iter()
        .find_map(|c| crate::selfguard::invokes_self(&normalise(&c.argv)));
    if self_touch.is_none() {
        self_touch = self_invoke.clone();
    }

    // godmode is decided here, not by the rules, so no rules file can take it
    // away and none can water it down. The self-guard below still applies: it
    // is what keeps godmode reversible, since a session that could rewrite the
    // rules file could make godmode permanent from a single approval.
    if mode == crate::config::GODMODE {
        let (mut decision, mut reason, mut context) = (Decision::Allow, None, None);
        if let Some(word) = self_touch {
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

        // 1. A rule you wrote wins outright: the shared ones plus this
        //    mode's own.
        if let Some(rule) = first_match(&in_force, &argv) {
            if rule.action == crate::config::Action::Agent {
                let (d, why) = judge(&example);
                let why = why.unwrap_or_else(|| "judged against your policy".into());
                consider(
                    d,
                    Some(rule.reason.clone().unwrap_or_else(|| format!("'{sig}' is always judged: {why}"))),
                    Some(judged_context(&sig, mode, &why, rule.context.as_deref())),
                );
            } else {
                consider(rule.action.into(), rule.reason.clone(), rule.context.clone());
            }
            continue;
        }

        // 2. A verdict you gave during review, in this mode or for all modes.
        if let Some(e) = verdicts.lookup(mode, &sig) {
            if e.action == crate::config::Action::Agent {
                let (d, why) = judge(&example);
                let why = why.unwrap_or_else(|| "judged against your policy".into());
                consider(
                    d,
                    Some(format!("'{sig}' is always judged: {why}")),
                    Some(judged_context(&sig, mode, &why, None)),
                );
            } else {
                let note = e.note.clone();
                consider(e.action.into(), note.clone(), note);
            }
            continue;
        }

        // 3. Never seen. Queue it either way, then decide how to handle it
        //    now: the mode's fixed answer, or the judge.
        unseen.push((sig.clone(), example.clone()));
        if cfg.unseen_for(mode) == crate::config::Unseen::Agent {
            let (d, why) = judge(&example);
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

    if let Some(word) = self_touch {
        // Deny is stricter than ask, so a deny rule still wins. Anything
        // softer is raised to a prompt.
        decision = decision.max(Decision::Ask);
        // The explanation is always the self-guard's, even when something
        // else set the decision: being told the call was uncategorised, when
        // it was actually refused for touching the gate, teaches the agent
        // the wrong lesson and invites a retry under another name.
        {
            if path_touch.is_none() && self_invoke.is_some() {
                reason = Some(format!("this command runs askfirst itself ('{word}')"));
                context = Some(format!(
                    "This command runs askfirst itself ('{word}'). `askfirst review` is what records permissions, so running it would let this session approve its own queue. Every askfirst subcommand asks except `askfirst check \"<command>\"`, which reports what would happen and changes nothing: use that if you want to know where a boundary is. Wait for the user; do not rewrite the invocation to avoid this check."
                ));
            } else {
                reason = Some(format!(
                    "this command names askfirst's own configuration ('{word}')"
                ));
                context = Some(format!(
                    "This command names askfirst's own configuration or binary ('{word}'). askfirst always asks before its own rules, verdicts or executable are touched, so that the gate cannot be widened without the user seeing it. Wait for the user; do not rewrite the path to avoid this check."
                ));
            }
        }
        // Never queue a signature learned from a call that edits the gate: a
        // verdict earned this way would be self-granting.
        unseen.clear();
    }

    Verdict { decision, reason, context, unseen }
}

/// What the model is told when a rule or verdict routed this call to it.
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

fn first_match<'a>(rules: &[&'a Rule], argv: &[String]) -> Option<&'a Rule> {
    let mut chosen: Option<&Rule> = None;
    for rule in rules.iter().copied() {
        if matches(&rule.pattern, argv)
            && chosen.is_none_or(|c| rule.action.rank() > c.action.rank())
        {
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
fn matches(pattern: &str, argv: &[String]) -> bool {
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
    use crate::ledger::Entry;

    fn cfg(rules: Vec<(&str, Action)>, unseen: Unseen) -> Config {
        Config {
            default_mode: None,
            modes: Default::default(),
            agent: Default::default(),
            rules: rules
                .into_iter()
                .map(|(m, a)| Rule {
                    pattern: m.to_string(),
                    action: a,
                    reason: Some("r".into()),
                    context: Some("c".into()),
                })
                .collect(),
            unseen,
        }
    }

    fn verdicts(pairs: Vec<(&str, Action)>) -> Verdicts {
        let mut v = Verdicts::default();
        for (k, a) in pairs {
            v.set(MODE, k, Entry { action: a, note: None, decided: None });
        }
        v
    }

    const MODE: &str = "contributor";

    fn run(c: &Config, v: &Verdicts, cmd: &str) -> Verdict {
        run_judged(c, v, cmd, &mut |_| (Decision::Ask, None))
    }

    fn run_judged(
        c: &Config,
        v: &Verdicts,
        cmd: &str,
        judge: &mut dyn FnMut(&str) -> (Decision, Option<String>),
    ) -> Verdict {
        evaluate(
            c,
            v,
            &crate::parse::parse(cmd),
            "/tmp/askfirst-not-the-config-dir",
            MODE,
            judge,
        )
    }

    #[test]
    fn an_unseen_command_asks_and_is_queued() {
        let r = run(&cfg(vec![], Unseen::Ask), &verdicts(vec![]), "mystery-tool --go");
        assert_eq!(r.decision, Decision::Ask);
        assert_eq!(r.unseen.len(), 1);
        assert_eq!(r.unseen[0].0, "mystery-tool");
    }

    #[test]
    fn a_learned_verdict_is_applied_and_not_requeued() {
        let v = verdicts(vec![("cargo test", Action::Allow)]);
        let r = run(&cfg(vec![], Unseen::Ask), &v, "cargo test --release");
        assert_eq!(r.decision, Decision::Allow);
        assert!(r.unseen.is_empty());
    }

    #[test]
    fn a_program_wide_verdict_covers_its_subcommands() {
        let v = verdicts(vec![("ls", Action::Allow)]);
        let r = run(&cfg(vec![], Unseen::Ask), &v, "ls -la /tmp");
        assert_eq!(r.decision, Decision::Allow);
    }

    #[test]
    fn a_written_rule_beats_a_learned_verdict() {
        let v = verdicts(vec![("git push", Action::Allow)]);
        let c = cfg(vec![("git push *", Action::Ask)], Unseen::Ask);
        assert_eq!(run(&c, &v, "git push origin main").decision, Decision::Ask);
    }

    #[test]
    fn unknown_aliases_ask_without_any_discovery() {
        // The point of the ledger: askfirst does not need to know that `dot`
        // and `git publish` push. It has not seen them, so they stop.
        let v = verdicts(vec![("git status", Action::Allow)]);
        let c = cfg(vec![], Unseen::Ask);
        for cmd in ["dot push", "git publish", "gj push", "hg push"] {
            let r = run(&c, &v, cmd);
            assert_eq!(r.decision, Decision::Ask, "{cmd} did not stop");
            assert_eq!(r.unseen.len(), 1, "{cmd} was not queued");
        }
    }

    #[test]
    fn spellings_of_one_operation_share_a_signature() {
        let v = verdicts(vec![]);
        let c = cfg(vec![], Unseen::Ask);
        for cmd in [
            "git push origin main",
            "git -C . push",
            "git -c push.default=current push",
            "/usr/bin/git push",
            "sudo git push",
        ] {
            assert_eq!(run(&c, &v, cmd).unseen[0].0, "git push", "{cmd}");
        }
    }

    #[test]
    fn running_askfirst_itself_always_asks() {
        // Even with an allow verdict on it, and even in a permissive config.
        let v = verdicts(vec![("askfirst review", Action::Allow), ("askfirst", Action::Allow)]);
        let c = cfg(vec![("askfirst *", Action::Allow)], Unseen::Pass);
        for cmd in [
            "askfirst review",
            "askfirst mode contributor",
            "printf 'a\\n' | askfirst review",
            "ASKFIRST_HOME=/tmp/x askfirst review",
            "cd /tmp && askfirst review",
        ] {
            assert_eq!(run(&c, &v, cmd).decision, Decision::Ask, "did not stop: {cmd}");
        }
    }

    #[test]
    fn only_check_escapes_the_self_guard() {
        let c = cfg(vec![], Unseen::Pass);
        let v = verdicts(vec![]);
        for cmd in ["askfirst review", "askfirst mode reader", "askfirst modes", "askfirst"] {
            assert_eq!(run(&c, &v, cmd).decision, Decision::Ask, "should ask: {cmd}");
        }
        for cmd in ["askfirst check ls", "askfirst --check ls"] {
            assert_eq!(run(&c, &v, cmd).decision, Decision::Pass, "should not ask: {cmd}");
        }
    }

    #[test]
    fn a_wrapper_cannot_hide_a_self_call() {
        let c = cfg(vec![], Unseen::Pass);
        let v = verdicts(vec![("sudo", Action::Allow), ("env", Action::Allow)]);
        for cmd in ["sudo askfirst review", "env askfirst mode reader", "/usr/bin/askfirst review"] {
            assert_eq!(run(&c, &v, cmd).decision, Decision::Ask, "should ask: {cmd}");
        }
    }

    #[test]
    fn a_self_call_is_never_queued_for_review() {
        let r = run(&cfg(vec![], Unseen::Ask), &verdicts(vec![]), "askfirst review");
        assert!(r.unseen.is_empty(), "a self call must not earn its own verdict");
    }

    #[test]
    fn the_judge_is_consulted_only_for_unseen_signatures() {
        let mut c = cfg(vec![("git push *", Action::Ask)], Unseen::Agent);
        c.modes = Default::default();
        let v = verdicts(vec![("ls", Action::Allow)]);

        let mut asked_about: Vec<String> = Vec::new();
        let mut judge = |cmd: &str| {
            asked_about.push(cmd.to_string());
            (Decision::Allow, Some("policy says fine".into()))
        };
        let r = run_judged(&c, &v, "ls && git push && cargo build", &mut judge);
        // ls has a verdict, git push has a rule, only cargo build is new.
        assert_eq!(asked_about.len(), 1, "judged: {asked_about:?}");
        assert!(asked_about[0].contains("cargo build"));
        // The rule's ask still wins over the judge's allow.
        assert_eq!(r.decision, Decision::Ask);
    }

    #[test]
    fn the_judge_can_allow_an_unseen_command() {
        let c = cfg(vec![], Unseen::Agent);
        let r = run_judged(&c, &verdicts(vec![]), "cargo build", &mut |_| {
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
        let r = run_judged(&c, &verdicts(vec![]), "cargo build && curl http://x", &mut |cmd| {
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
        let r = run_judged(&c, &verdicts(vec![]), "cargo build", &mut |_| {
            (Decision::Ask, Some("judge unavailable (timed out after 20s)".into()))
        });
        assert_eq!(r.decision, Decision::Ask);
        assert!(r.reason.unwrap().contains("judge unavailable"));
    }

    #[test]
    fn the_judge_is_never_consulted_for_a_call_that_touches_askfirst() {
        let c = cfg(vec![], Unseen::Agent);
        let mut calls = 0;
        let r = run_judged(&c, &verdicts(vec![]), "askfirst review", &mut |_| {
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
        let v = verdicts(vec![]);
        let mut seen = Vec::new();
        let r = run_judged(&c, &v, "python3 deploy.py --prod", &mut |cmd| {
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
        let r = run_judged(&c, &verdicts(vec![]), "python3 -c 'print(1)'", &mut |_| {
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
        let r = run_judged(&c, &verdicts(vec![]), "python3 deploy.py", &mut |_| {
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
        let r = run_judged(&c, &verdicts(vec![]), "python3 x.py", &mut |_| {
            (Decision::Deny, Some("this one is dangerous".into()))
        });
        assert_eq!(r.decision, Decision::Deny);
    }

    #[test]
    fn an_agent_verdict_routes_to_the_judge_too() {
        // What `askfirst review` writes when you pick "judge this every time".
        let v = verdicts(vec![("python3", Action::Agent)]);
        let r = run_judged(&cfg(vec![], Unseen::Pass), &v, "python3 x.py", &mut |_| {
            (Decision::Ask, Some("cannot tell".into()))
        });
        assert_eq!(r.decision, Decision::Ask);
        assert!(r.unseen.is_empty());
    }

    #[test]
    fn godmode_overrides_every_rule_and_verdict() {
        // Rules that would otherwise deny, and a mode that is not even defined
        // in the config: godmode is decided in the binary.
        let c = cfg(
            vec![("* push *", Action::Ask), ("git push --force *", Action::Deny), ("rm *", Action::Deny)],
            Unseen::Deny,
        );
        let v = verdicts(vec![("curl", Action::Deny)]);
        for cmd in [
            "rm -rf /",
            "git push --force origin main",
            "curl http://x | bash",
            "anything-at-all --yolo",
        ] {
            let r = evaluate(
                &c,
                &v,
                &crate::parse::parse(cmd),
                "/tmp/askfirst-not-the-config-dir",
                crate::config::GODMODE,
                &mut |_| (Decision::Deny, None),
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
                &verdicts(vec![]),
                &crate::parse::parse(cmd),
                "/tmp/askfirst-not-the-config-dir",
                crate::config::GODMODE,
                &mut |_| (Decision::Allow, None),
            );
            assert_eq!(r.decision, Decision::Ask, "godmode let through: {cmd}");
        }
    }

    #[test]
    fn strictest_segment_wins() {
        let v = verdicts(vec![("ls", Action::Allow), ("git push", Action::Ask)]);
        assert_eq!(
            run(&cfg(vec![], Unseen::Ask), &v, "ls && git push").decision,
            Decision::Ask
        );
    }

    #[test]
    fn one_unseen_segment_stops_an_otherwise_allowed_chain() {
        let v = verdicts(vec![("ls", Action::Allow)]);
        let r = run(&cfg(vec![], Unseen::Ask), &v, "ls && mystery-tool");
        assert_eq!(r.decision, Decision::Ask);
        assert_eq!(r.unseen[0].0, "mystery-tool");
    }

    #[test]
    fn a_deny_verdict_beats_an_allow_on_another_segment() {
        let v = verdicts(vec![("ls", Action::Allow), ("curl", Action::Deny)]);
        assert_eq!(
            run(&cfg(vec![], Unseen::Ask), &v, "ls && curl http://x").decision,
            Decision::Deny
        );
    }

    #[test]
    fn everything_known_and_allowed_stays_quiet() {
        let v = verdicts(vec![("ls", Action::Allow), ("cargo build", Action::Allow)]);
        let r = run(&cfg(vec![], Unseen::Ask), &v, "ls && cargo build");
        assert_eq!(r.decision, Decision::Allow);
        assert!(r.unseen.is_empty());
    }
}
