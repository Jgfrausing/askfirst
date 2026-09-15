//! askfirst: a self-evolving PreToolUse hook for Claude Code.
//!
//! It does not try to work out what a command does. It reduces the call to a
//! signature, applies whatever you decided about that signature in the mode
//! you are in, and asks about anything it has not seen. What it asked about
//! joins a queue you work through with `askfirst review`.
//!
//! Modes are the posture you are working in: `reader` investigates,
//! `contributor` changes things. Switching mode changes what runs without
//! asking, the way Claude Code's own permission modes do.
//!
//! Failure escalates. A malformed rule file, an unparseable command or an
//! unreadable event all end in `ask` rather than silence, because a gate that
//! fails open is worse than no gate: it looks like it is working.

mod agent;
mod config;
mod hook;
mod ledger;
mod parse;
mod rules;
mod selfguard;

use config::Action;

/// Tools that write a file directly. They never reach a shell, so the command
/// parsing does not apply; only their path matters.
const FILE_TOOLS: &[&str] = &["Edit", "Write", "MultiEdit", "NotebookEdit"];
use hook::Decision;
use std::io::{BufRead, Read, Write};
use std::path::PathBuf;

fn main() {
    // Rust ignores SIGPIPE, so `askfirst pending | head` panics instead of
    // ending quietly. Restore the default so this behaves like other CLIs.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    let args: Vec<String> = std::env::args().skip(1).collect();
    let rest: Vec<String> = args.iter().skip(1).cloned().collect();
    match args.first().map(String::as_str) {
        Some("mode") => std::process::exit(mode_cmd(
            rest.iter().find(|a| !a.starts_with("--")).map(String::as_str),
            rest.iter().any(|a| a == "--global"),
            rest.iter().any(|a| a == "--create"),
        )),
        Some("defaultmode") => std::process::exit(default_mode_cmd(
            rest.first().map(String::as_str),
        )),
        Some("install") => std::process::exit(install_cmd()),
        Some("modes") => std::process::exit(modes_cmd()),
        Some("review") => {
            let mode = flag_value(&rest, "--mode");
            if rest.iter().any(|a| a == "--agent") {
                std::process::exit(review_with_agent(mode))
            }
            std::process::exit(review(mode))
        }
        Some("pending") => std::process::exit(show_pending(flag_value(&rest, "--mode"))),
        Some("check") | Some("--check") => {
            std::process::exit(check(&rest.first().cloned().unwrap_or_default()))
        }
        Some("paths") => {
            println!("dir:      {}", config::dir().display());
            println!("rules:    {}", config::default_path().display());
            println!("verdicts: {}", ledger::verdicts_path().display());
            println!("pending:  {}", ledger::pending_path().display());
            println!("state:    {}", ledger::state_path().display());
            println!("policy:   {}", agent::policy_path().display());
            println!("sessions: {}", ledger::sessions_path().display());
            std::process::exit(0);
        }
        Some("--help") | Some("-h") => {
            eprintln!(
                "askfirst: a self-evolving PreToolUse hook for Claude Code.\n\n\
                 With no arguments it reads a hook event on stdin and writes a decision\n\
                 on stdout. A command it has not seen asks, and joins the review queue.\n\n\
                 askfirst mode                 print the mode in force\n\
                 askfirst mode <name>          switch mode for this session\n\
                 askfirst mode <name> --global switch mode for every session\n\
                 askfirst mode <name> --create create the mode, then switch to it\n\
                 askfirst defaultmode <name>   set the mode new sessions start in\n\
                 askfirst install              install the /askfirst skill\n\
                 askfirst modes                list the modes and what is waiting in each\n\
                 askfirst review [--mode M]    categorise what is waiting\n\
                 askfirst review --agent       let the policy judge the queue, you keep the rest\n\
                 askfirst pending [--mode M]   list what is waiting, without deciding\n\
                 askfirst check \"<cmd>\"        explain what would happen to one command\n\
                 askfirst paths                print the files it uses\n\n\
                 Everything lives in one directory; `askfirst paths` prints it.\n\
                 ASKFIRST_HOME moves that directory, ASKFIRST_MODE overrides the mode."
            );
            std::process::exit(0);
        }
        _ => {}
    }
    run_hook();
}

fn flag_value(args: &[String], flag: &str) -> Option<String> {
    let i = args.iter().position(|a| a == flag)?;
    args.get(i + 1).cloned()
}

fn load_cfg_or_exit() -> config::Config {
    match config::load(&config::default_path()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("rule file error: {e}");
            std::process::exit(2);
        }
    }
}

fn run_hook() {
    let mut raw = String::new();
    if std::io::stdin().read_to_string(&mut raw).is_err() {
        return escalate("askfirst could not read the hook event");
    }
    let input: hook::HookInput = match serde_json::from_str(&raw) {
        Ok(i) => i,
        // Not an event we understand. Stay silent rather than block a session
        // on our own bug.
        Err(_) => return,
    };
    // The file tools reach askfirst's own files without a shell, which the
    // command parsing above can never see. Gate them on the path alone.
    if FILE_TOOLS.contains(&input.tool_name.as_str()) {
        if let Some(path) = input.tool_input.path() {
            if let Some(word) = selfguard::file_tool_touches_self(path, &input.cwd) {
                return escalate(&format!(
                    "this would change askfirst's own configuration ('{word}') with the {} tool",
                    input.tool_name
                ));
            }
        }
        return;
    }
    if input.tool_name != "Bash" {
        return;
    }
    let Some(command) = input.tool_input.command.as_deref() else {
        return;
    };

    let cfg = match config::load(&config::default_path()) {
        Ok(c) => c,
        Err(e) => return escalate(&format!("askfirst could not read its rules ({e})")),
    };
    let parsed = parse::parse(command);
    if parsed.had_error {
        return escalate(
            "askfirst could not parse this command, so it cannot be checked against the rules",
        );
    }

    ledger::record_session(&input.session_id);
    let mode = ledger::active_mode_for(&cfg, &input.session_id);
    // A mode that is set but not defined would silently fall back to the
    // shared rules, which is looser than the mode you thought you were in.
    if !mode.is_empty() && !cfg.knows_mode(&mode) {
        return escalate(&format!(
            "askfirst is set to mode '{mode}', which is not defined in its rules"
        ));
    }

    let verdicts = ledger::load_verdicts();
    let mut judge = |command: &str| agent::judge(&cfg.agent, &mode, command, &input.cwd);
    let v = rules::evaluate(&cfg, &verdicts, &parsed, &input.cwd, &mode, &mut judge);

    // Record what we had no decision for. This is an observation, never a
    // permission: only `askfirst review` writes a verdict.
    for (sig, example) in &v.unseen {
        ledger::record_sighting(sig, example, &mode);
    }

    if let Some(out) = hook::render(v.decision, v.reason, v.context) {
        println!("{out}");
    }
}

fn escalate(why: &str) {
    if let Some(out) = hook::render(
        Decision::Ask,
        Some(why.to_string()),
        Some(format!("{why}. Wait for the user rather than rephrasing the command.")),
    ) {
        println!("{out}");
    }
}

fn mode_cmd(name: Option<&str>, global: bool, create: bool) -> i32 {
    let cfg = load_cfg_or_exit();
    let Some(name) = name else {
        let current = ledger::active_mode(&cfg);
        if current.is_empty() {
            println!("no mode set; only the shared rules apply");
        } else {
            let desc = cfg
                .modes
                .get(&current)
                .and_then(|m| m.description.clone())
                .unwrap_or_default();
            println!(
                "{current}{}",
                if desc.is_empty() { String::new() } else { format!("  {desc}") }
            );
            if !cfg.knows_mode(&current) {
                println!(
                    "warning: '{current}' is not defined in {}",
                    config::default_path().display()
                );
            }
        }
        return 0;
    };

    if !cfg.knows_mode(name) {
        if !create {
            // Exit 3 is the signal the skill branches on: the mode is not a
            // typo to reject, it is something the user may want created.
            eprintln!("no mode '{name}'.");
            if cfg.modes.is_empty() {
                eprintln!("No modes are defined yet.");
            } else {
                eprintln!("Defined modes:");
                for (m, def) in &cfg.modes {
                    eprintln!("  {m}  {}", def.description.clone().unwrap_or_default());
                }
            }
            eprintln!("Create it with: askfirst mode {name} --create");
            return 3;
        }
        if let Err(e) = config::create_mode(name) {
            eprintln!("could not create mode '{name}': {e}");
            return 1;
        }
        println!("created mode '{name}' in {}", config::default_path().display());
        println!("it starts with unseen = \"ask\"; edit the rules file to fill it in");
    }

    if global {
        if let Err(e) = ledger::set_mode(name) {
            eprintln!("could not set mode: {e}");
            return 1;
        }
        println!("mode is now {name} for every session");
    } else {
        let Some(session) = ledger::last_session() else {
            eprintln!(
                "askfirst does not know which session this is: no hook event has been seen yet."
            );
            eprintln!("Run a command first, or use --global to set it everywhere.");
            return 1;
        };
        if let Err(e) = ledger::set_session_mode(&session, name, &today()) {
            eprintln!("could not set mode: {e}");
            return 1;
        }
        println!("mode is now {name} for this session");
    }

    let waiting = ledger::pending(name).len();
    if waiting > 0 {
        println!("{waiting} signature(s) waiting in this mode; `askfirst review` to categorise");
    }
    0
}

/// Set the mode new sessions start in. Refuses an unknown mode: unlike
/// `askfirst mode`, this writes a lasting default, so a typo would leave every
/// future session in a mode that does not exist.
fn default_mode_cmd(name: Option<&str>) -> i32 {
    let cfg = load_cfg_or_exit();
    let Some(name) = name else {
        match cfg.default_mode.as_deref() {
            Some(m) => println!("{m}"),
            None => println!("no default mode set"),
        }
        return 0;
    };
    if !cfg.knows_mode(name) {
        eprintln!("no mode '{name}', so it cannot be the default.");
        if !cfg.modes.is_empty() {
            eprintln!("Defined modes:");
            for m in cfg.modes.keys() {
                eprintln!("  {m}");
            }
        }
        eprintln!("Create it first with: askfirst mode {name} --create");
        return 3;
    }
    if let Err(e) = config::set_default_mode(name) {
        eprintln!("could not set the default mode: {e}");
        return 1;
    }
    println!("new sessions will start in {name}");
    0
}


/// The files `askfirst install` writes when they do not exist.
///
/// Generated rather than shipped separately so a fresh machine is one command,
/// and so the defaults cannot drift from what the binary understands.
const DEFAULT_RULES: &str = r##"{
  "//": "askfirst rules. Keys starting with // are comments; askfirst ignores them.",
  "//actions": "allow | ask | deny | pass | agent. Firmness: allow < pass < agent < ask < deny.",
  "//unseen": "What happens to a signature with no rule and no verdict: ask (queue it, instant), agent (judge it, 6-8s), deny, or pass.",
  "default_mode": "contributor",
  "unseen": "ask",
  "agent": {
    "//": "The judge for action=agent, unseen=agent and `askfirst review --agent`.",
    "//timeout": "Keep the PreToolUse hook timeout in settings.json comfortably above this.",
    "model": "haiku",
    "timeout_secs": 20
  },
  "//rules": "Apply in every mode. A mode may tighten these, never loosen them.",
  "rules": [
    {
      "match": "* push *",
      "action": "ask",
      "reason": "Pushing publishes work. Confirm the remote and branch.",
      "context": "The user requires a prompt before any push, whatever the program or alias. Wait for their answer; do not retry through a script, a wrapper or a different spelling."
    },
    {
      "match": "git push --force *",
      "action": "deny",
      "reason": "Force push rewrites published history.",
      "context": "Force push is denied. Use --force-with-lease and ask the user first, or rebase locally and push normally."
    },
    {
      "match": "git push -f *",
      "action": "deny",
      "reason": "Force push rewrites published history.",
      "context": "Force push is denied. Use --force-with-lease and ask the user first, or rebase locally and push normally."
    },
    {
      "match": "python3 *",
      "action": "agent"
    },
    {
      "match": "python *",
      "action": "agent"
    },
    {
      "match": "node *",
      "action": "agent"
    },
    {
      "match": "npx *",
      "action": "agent"
    },
    {
      "match": "uvx *",
      "action": "agent"
    }
  ],
  "modes": {
    "reader": {
      "description": "Read and investigate. Anything that changes state is refused.",
      "unseen": "deny",
      "rules": [
        {
          "match": "git status *",
          "action": "allow"
        },
        {
          "match": "git log *",
          "action": "allow"
        },
        {
          "match": "git diff *",
          "action": "allow"
        },
        {
          "match": "git show *",
          "action": "allow"
        },
        {
          "match": "git blame *",
          "action": "allow"
        },
        {
          "match": "cargo tree *",
          "action": "allow"
        },
        {
          "match": "cargo metadata *",
          "action": "allow"
        },
        {
          "match": "git commit *",
          "action": "deny",
          "reason": "reader mode does not commit.",
          "context": "askfirst is in reader mode, which investigates without changing anything. Report what you found and let the user switch to contributor mode if they want the change made."
        },
        {
          "match": "git add *",
          "action": "deny",
          "reason": "reader mode does not stage changes.",
          "context": "askfirst is in reader mode. Do not stage or commit; describe the change you would make instead."
        },
        {
          "match": "* push *",
          "action": "deny",
          "reason": "reader mode does not publish anything.",
          "context": "askfirst is in reader mode, which never publishes. Do not push. Tell the user what you would have pushed."
        }
      ]
    },
    "contributor": {
      "description": "Edit, build, test and commit. Publishing and discarding still ask.",
      "unseen": "ask",
      "rules": [
        {
          "match": "cargo build *",
          "action": "allow"
        },
        {
          "match": "cargo test *",
          "action": "allow"
        },
        {
          "match": "cargo check *",
          "action": "allow"
        },
        {
          "match": "cargo clippy *",
          "action": "allow"
        },
        {
          "match": "cargo fmt *",
          "action": "allow"
        },
        {
          "match": "git add *",
          "action": "allow"
        },
        {
          "match": "git commit *",
          "action": "allow"
        },
        {
          "match": "git checkout *",
          "action": "allow"
        },
        {
          "match": "git switch *",
          "action": "allow"
        },
        {
          "match": "git fetch *",
          "action": "allow"
        },
        {
          "match": "git pull *",
          "action": "allow"
        },
        {
          "match": "git reset --hard *",
          "action": "ask",
          "reason": "This discards uncommitted work.",
          "context": "A hard reset throws away uncommitted changes. Confirm with the user what would be lost before running it."
        },
        {
          "match": "git clean *",
          "action": "ask",
          "reason": "This deletes untracked files."
        },
        {
          "match": "git stash *",
          "action": "ask",
          "reason": "Stashing can hide uncommitted work."
        }
      ]
    },
    "godmode": {
      "description": "Everything runs. Shared rules and the self-guard still apply.",
      "unseen": "pass",
      "rules": [
        {
          "match": "*",
          "action": "allow"
        }
      ],
      "//": "A catch-all allow. The shared rules still apply (a push still asks, a force push is still denied) and the self-guard is compiled in, so askfirst's own config still asks."
    }
  }
}
"##;

const DEFAULT_POLICY: &str = r##"# askfirst judging policy, in plain English.
#
# Used when a mode sets `unseen = "agent"`, and by `askfirst review --agent`.
# A judge that cannot tell from this text answers `ask`, so write what you mean
# and leave the rest to be prompted. An incomplete policy is safe; a vague one
# is not.
#
# In the hook, the verdict decides one call and is never written down. In
# `askfirst review --agent` a confident allow or deny is recorded, and anything
# unsettled is left for you.

# Prepended to every mode.
shared = """
Never run anything that changes askfirst's own configuration, the shell
profile, or the Claude Code settings. Never disable a safety check, a hook, a
test that guards security behaviour, or a certificate check. Anything that
sends the contents of this machine somewhere else is a deny.
"""

[modes]

contributor = """
Agents should be allowed to pull, commit, and edit files. Discarding changes
and pushing should be prompted first. In general it should not be able to drop
existing local changes or make changes remotely.

Allow: reading and editing files in the working directory, building, running
tests and linters, formatting, installing dependencies already declared in a
lock file or manifest, staging and committing, creating branches, fetching and
pulling.

Ask: pushing, publishing, opening or merging a pull request, anything that
discards uncommitted work (git reset --hard, git checkout -- ., git clean,
git stash drop), deleting files that existed before this session, adding a new
dependency to a manifest, anything that writes outside the working directory,
anything touching a database.

Deny: force pushing, rewriting published history, deleting a remote branch or
tag, deploying, applying infrastructure changes, granting permissions, writing
to a secret store, printing a credential.
"""

reader = """
Should only be allowed to read local files and access remote resources as a
reader.

Allow: listing and reading files, searching, git status, git log, git diff,
git show, git blame, read-only inspection of dependencies, read-only HTTP
requests to documentation and package registries.

Ask: anything that reads outside the working directory, anything that starts a
long-running process or a server, anything whose effect cannot be determined
from the command alone.

Deny: writing or deleting any file, staging, committing, pushing, installing
anything, starting or stopping a service, any request that sends data rather
than retrieving it.
"""
"##;

const SEED_VERDICTS: &str = r##"{
  "//": "Seeded with the read-only commands Claude Code already runs without a prompt, so askfirst does not start out noisier than no hook at all. Anything that can write (sed -i, awk, find -delete, xargs, env) is left out on purpose and will come up in review.",
  "modes": {
    "*": {
      "entries": {
        "basename": {
          "action": "allow",
          "decided": "2026-09-15"
        },
        "cat": {
          "action": "allow",
          "decided": "2026-09-15"
        },
        "cmp": {
          "action": "allow",
          "decided": "2026-09-15"
        },
        "column": {
          "action": "allow",
          "decided": "2026-09-15"
        },
        "cut": {
          "action": "allow",
          "decided": "2026-09-15"
        },
        "date": {
          "action": "allow",
          "decided": "2026-09-15"
        },
        "df": {
          "action": "allow",
          "decided": "2026-09-15"
        },
        "diff": {
          "action": "allow",
          "decided": "2026-09-15"
        },
        "dirname": {
          "action": "allow",
          "decided": "2026-09-15"
        },
        "du": {
          "action": "allow",
          "decided": "2026-09-15"
        },
        "echo": {
          "action": "allow",
          "decided": "2026-09-15"
        },
        "false": {
          "action": "allow",
          "decided": "2026-09-15"
        },
        "file": {
          "action": "allow",
          "decided": "2026-09-15"
        },
        "grep": {
          "action": "allow",
          "decided": "2026-09-15"
        },
        "groups": {
          "action": "allow",
          "decided": "2026-09-15"
        },
        "head": {
          "action": "allow",
          "decided": "2026-09-15"
        },
        "hostname": {
          "action": "allow",
          "decided": "2026-09-15"
        },
        "id": {
          "action": "allow",
          "decided": "2026-09-15"
        },
        "jq": {
          "action": "allow",
          "decided": "2026-09-15"
        },
        "less": {
          "action": "allow",
          "decided": "2026-09-15"
        },
        "locale": {
          "action": "allow",
          "decided": "2026-09-15"
        },
        "ls": {
          "action": "allow",
          "decided": "2026-09-15"
        },
        "man": {
          "action": "allow",
          "decided": "2026-09-15"
        },
        "md5": {
          "action": "allow",
          "decided": "2026-09-15"
        },
        "printf": {
          "action": "allow",
          "decided": "2026-09-15"
        },
        "pwd": {
          "action": "allow",
          "decided": "2026-09-15"
        },
        "realpath": {
          "action": "allow",
          "decided": "2026-09-15"
        },
        "rg": {
          "action": "allow",
          "decided": "2026-09-15"
        },
        "shasum": {
          "action": "allow",
          "decided": "2026-09-15"
        },
        "sleep": {
          "action": "allow",
          "decided": "2026-09-15"
        },
        "sort": {
          "action": "allow",
          "decided": "2026-09-15"
        },
        "stat": {
          "action": "allow",
          "decided": "2026-09-15"
        },
        "tail": {
          "action": "allow",
          "decided": "2026-09-15"
        },
        "tput": {
          "action": "allow",
          "decided": "2026-09-15"
        },
        "tr": {
          "action": "allow",
          "decided": "2026-09-15"
        },
        "tree": {
          "action": "allow",
          "decided": "2026-09-15"
        },
        "true": {
          "action": "allow",
          "decided": "2026-09-15"
        },
        "type": {
          "action": "allow",
          "decided": "2026-09-15"
        },
        "uname": {
          "action": "allow",
          "decided": "2026-09-15"
        },
        "uniq": {
          "action": "allow",
          "decided": "2026-09-15"
        },
        "uptime": {
          "action": "allow",
          "decided": "2026-09-15"
        },
        "wc": {
          "action": "allow",
          "decided": "2026-09-15"
        },
        "which": {
          "action": "allow",
          "decided": "2026-09-15"
        },
        "whoami": {
          "action": "allow",
          "decided": "2026-09-15"
        }
      }
    }
  }
}
"##;

/// Write a file only if it is not already there. An existing rules file is
/// someone's policy; overwriting it would be the one thing this tool exists to
/// prevent.
fn write_if_absent(path: &std::path::Path, body: &str, what: &str) -> bool {
    if path.exists() {
        println!("  kept     {} ({what} already exists)", path.display());
        return false;
    }
    if let Some(d) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(d) {
            println!("  FAILED   {}: {e}", d.display());
            return false;
        }
    }
    match std::fs::write(path, body) {
        Ok(()) => {
            println!("  wrote    {}", path.display());
            true
        }
        Err(e) => {
            println!("  FAILED   {}: {e}", path.display());
            false
        }
    }
}

const SKILL: &str = r#"---
name: askfirst
description: >-
  Switch the askfirst permission mode for this session, or create a mode.
  Use when the user types /askfirst <mode>, or says "switch to reader mode",
  "go read-only", "put askfirst in contributor mode", or asks which mode is
  in force. askfirst gates every Bash command, so the mode decides what runs
  without asking.
---

# askfirst mode

askfirst is a PreToolUse hook that decides allow / ask / deny for every Bash
command. A mode is the posture it applies: `reader` investigates, `contributor`
changes things. This skill switches it for the current session only.

## Switching

Run the mode the user asked for:

```sh
askfirst mode <name>
```

Every askfirst command asks for permission before it runs, which is deliberate:
the user sees each change to the gate. Wait for their approval rather than
rephrasing the command.

Exit codes:

- `0`: the mode is set. Tell the user which mode is now in force.
- `3`: no such mode. The output lists the modes that do exist. Do not invent
  one and do not retry. Show the user the list and ask whether they want a mode
  named `<name>` created. Only if they say yes:

  ```sh
  askfirst mode <name> --create
  ```

  That appends a mode to the rules file with `unseen = "ask"` and a stub
  section to `policy.md`. Tell the user it is empty and will ask about
  everything until they fill it in.
- `1`: something failed. Show the user the error; do not work around it.

## Other things the user may mean

- "what mode am I in": `askfirst mode`
- "what modes are there": `askfirst modes`
- "make reader the default for new sessions": `askfirst defaultmode reader`,
  which fails if the mode does not exist
- "set it everywhere, not just here": `askfirst mode <name> --global`

## What not to do

Do not edit `~/.config/askfirst/` directly to change a mode. Those files are
guarded and the user has to approve any change to them; the commands above are
the supported route.

Do not run `askfirst review`. That is how the user records permissions, and
running it from a session would let the session approve its own queue.
"#;

fn install_cmd() -> i32 {
    println!("Config in {}:", config::dir().display());
    write_if_absent(&config::default_path(), DEFAULT_RULES, "rules");
    write_if_absent(&agent::policy_path(), DEFAULT_POLICY, "policy");
    write_if_absent(&ledger::verdicts_path(), SEED_VERDICTS, "verdicts");

    let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
    let skill = home.join(".claude/skills/askfirst/SKILL.md");
    println!("\nSkill:");
    if let Some(d) = skill.parent() {
        if let Err(e) = std::fs::create_dir_all(d) {
            println!("  FAILED   {}: {e}", d.display());
            return 1;
        }
    }
    // The skill is ours to keep current, so this one is always rewritten.
    match std::fs::write(&skill, SKILL) {
        Ok(()) => println!("  wrote    {}", skill.display()),
        Err(e) => {
            println!("  FAILED   {}: {e}", skill.display());
            return 1;
        }
    }

    let cfg = config::load(&config::default_path()).unwrap_or_default();
    println!("\nModes:");
    if cfg.modes.is_empty() {
        println!("  none defined; create one with `askfirst mode <name> --create`");
    } else {
        for (name, def) in &cfg.modes {
            println!("  {name:<14}{}", def.description.clone().unwrap_or_default());
        }
    }

    println!("\nNext:");
    println!("  1. Register the hook in ~/.claude/settings.json (or settings.local.json):");
    println!(
        "     {{\"hooks\":{{\"PreToolUse\":[{{\"matcher\":\"Bash|Edit|Write|MultiEdit|NotebookEdit\",\"hooks\":[{{\"type\":\"command\",\"command\":\"{}\",\"args\":[],\"timeout\":30}}]}}]}}",
        std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "askfirst".into())
    );
    println!("     The timeout must exceed agent.timeout_secs, or a judged call is killed");
    println!("     mid-answer and the tool call proceeds ungated.");
    println!("  2. Deny the agent write access to this config, or the gate is advisory:");
    println!(
        "     \"permissions\": {{\"deny\": [\"Edit({0}/**)\", \"Write({0}/**)\"]}}",
        config::dir().display()
    );
    println!("  3. Restart Claude Code so /askfirst appears.");
    0
}

fn modes_cmd() -> i32 {
    let cfg = load_cfg_or_exit();
    let current = ledger::active_mode(&cfg);
    if cfg.modes.is_empty() {
        println!("no modes defined in {}", config::default_path().display());
    }
    let waiting: std::collections::BTreeMap<String, usize> =
        ledger::pending_modes().into_iter().collect();
    for (name, def) in &cfg.modes {
        let marker = if *name == current { "*" } else { " " };
        let desc = def.description.clone().unwrap_or_default();
        let n = waiting.get(name).copied().unwrap_or(0);
        let tail = if n > 0 { format!("   ({n} waiting)") } else { String::new() };
        println!("{marker} {name:<14}{desc}{tail}");
    }
    if !cfg.modes.contains_key(config::GODMODE) {
        let marker = if current == config::GODMODE { "*" } else { " " };
        println!(
            "{marker} {:<14}Allows everything. Built in; always available.",
            config::GODMODE
        );
    }
    0
}

fn show_pending(mode: Option<String>) -> i32 {
    let cfg = load_cfg_or_exit();
    let mode = mode.unwrap_or_else(|| ledger::active_mode(&cfg));
    let items = ledger::pending(&mode);
    if items.is_empty() {
        println!("Nothing waiting in mode '{mode}'.");
        let other: Vec<String> = ledger::pending_modes()
            .into_iter()
            .filter(|(m, n)| *m != mode && *n > 0)
            .map(|(m, n)| format!("{m} ({n})"))
            .collect();
        if !other.is_empty() {
            println!("Waiting elsewhere: {}", other.join(", "));
        }
        return 0;
    }
    println!("{} signature(s) waiting in mode '{mode}':\n", items.len());
    for (sig, example, n) in &items {
        println!("  {sig}   (seen {n}x, e.g. {example})");
    }
    println!("\nRun `askfirst review` to categorise them.");
    0
}

/// Walk the queue and write down what you decide.
///
/// This is the only thing that writes a verdict. The hook observes; you decide.
fn review(mode: Option<String>) -> i32 {
    let cfg = load_cfg_or_exit();
    let mode = mode.unwrap_or_else(|| ledger::active_mode(&cfg));
    let items = ledger::pending(&mode);
    if items.is_empty() {
        return show_pending(Some(mode));
    }

    let mut verdicts = ledger::load_verdicts();
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    let total = items.len();
    let mut decided = 0usize;

    println!(
        "{total} signature(s) to categorise in mode '{mode}'.\n\
         a allow, s ask each time, d deny, p pass to Claude Code, j judge each time\n\
         b broaden to the program, g apply to every mode, k skip, q quit and save\n"
    );

    for (i, (sig, example, n)) in items.iter().enumerate() {
        let mut target = sig.clone();
        let mut scope = mode.clone();
        loop {
            let where_ = if scope == ledger::ALL_MODES {
                "every mode".to_string()
            } else {
                format!("mode {scope}")
            };
            println!("[{}/{total}] {target}   ({where_})", i + 1);
            println!("          seen {n}x, e.g. {example}");
            print!("          a/s/d/p/j/b/g/k/q > ");
            let _ = std::io::stdout().flush();

            let Some(Ok(line)) = lines.next() else {
                println!("\nno more input, saving what you decided");
                return finish(&verdicts, decided);
            };
            let action = match line.trim() {
                "a" => Action::Allow,
                "s" => Action::Ask,
                "d" => Action::Deny,
                "p" => Action::Pass,
                "j" => Action::Agent,
                "b" => {
                    let broadened = ledger::broaden(&target);
                    if broadened == target {
                        println!("          already program-wide");
                    } else {
                        println!("          now deciding for every {broadened} subcommand");
                        target = broadened;
                    }
                    continue;
                }
                "g" => {
                    scope = ledger::ALL_MODES.to_string();
                    println!("          this decision will apply in every mode");
                    continue;
                }
                "k" => break,
                "q" => return finish(&verdicts, decided),
                other => {
                    println!("          '{other}'? use a, s, d, p, j, b, g, k or q");
                    continue;
                }
            };
            verdicts.set(
                &scope,
                &target,
                ledger::Entry { action, note: None, decided: Some(today()) },
            );
            decided += 1;
            println!("          {target} -> {} ({where_})\n", action.label());
            break;
        }
    }
    finish(&verdicts, decided)
}

/// Resolve the queue with the same judge the `agent` unseen mode uses.
///
/// Only a confident `allow` or `deny` is written. Anything the policy does not
/// settle, and anything that fails, stays in the queue and is listed at the
/// end, because the point of the queue is that you decide what the policy did
/// not. This writes verdicts from a model's answers, which the hook itself
/// never does: it is one deliberate command, not something a session can do.
fn review_with_agent(mode: Option<String>) -> i32 {
    let cfg = load_cfg_or_exit();
    let mode = mode.unwrap_or_else(|| ledger::active_mode(&cfg));
    let items = ledger::pending(&mode);
    if items.is_empty() {
        return show_pending(Some(mode));
    }

    // Fail before spending anything if the policy cannot answer for this mode.
    match agent::load_policy() {
        Ok(p) => {
            if agent::policy_for(&p, &mode).is_none() {
                eprintln!(
                    "{} has no entry for mode '{mode}', so the judge has nothing to judge against.",
                    agent::policy_path().display()
                );
                return 2;
            }
        }
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    }

    println!(
        "Judging {} signature(s) in mode '{mode}' against {} using {}.\n",
        items.len(),
        agent::policy_path().display(),
        cfg.agent.model
    );

    let mut verdicts = ledger::load_verdicts();
    let mut decided = 0usize;
    let mut left: Vec<(String, String)> = Vec::new();

    for (sig, example, n) in &items {
        let (d, why) = agent::judge(&cfg.agent, &mode, example, "");
        let action = match d {
            Decision::Allow => Some(Action::Allow),
            Decision::Deny => Some(Action::Deny),
            // Ask is the judge declining to decide, which is the answer it is
            // told to give when unsure. Leave it for a person.
            _ => None,
        };
        match action {
            Some(a) => {
                verdicts.set(
                    &mode,
                    sig,
                    ledger::Entry {
                        action: a,
                        note: why.clone(),
                        decided: Some(today()),
                    },
                );
                decided += 1;
                println!("  {:<28} {}", sig, a.label());
            }
            None => {
                let reason = why.unwrap_or_else(|| "not settled by the policy".into());
                println!("  {:<28} left  ({reason})", sig);
                left.push((sig.clone(), format!("seen {n}x, e.g. {example}")));
            }
        }
    }

    if let Err(e) = ledger::save_verdicts(&verdicts) {
        eprintln!("could not write verdicts: {e}");
        return 1;
    }
    if let Err(e) = ledger::prune_pending() {
        eprintln!("could not prune the queue: {e}");
    }

    println!(
        "\n{decided} decided by the policy, {} left for you.",
        left.len()
    );
    if !left.is_empty() {
        println!("\nStill waiting in mode '{mode}':");
        for (sig, detail) in &left {
            println!("  {sig}   ({detail})");
        }
        println!("\nRun `askfirst review` to work through them.");
    }
    0
}

fn finish(verdicts: &ledger::Verdicts, decided: usize) -> i32 {
    if let Err(e) = ledger::save_verdicts(verdicts) {
        eprintln!("could not write verdicts: {e}");
        return 1;
    }
    if let Err(e) = ledger::prune_pending() {
        eprintln!("could not prune the queue: {e}");
    }
    println!(
        "{decided} decision(s) written to {}",
        ledger::verdicts_path().display()
    );
    0
}

/// A date stamp without pulling in a time crate.
fn today() -> String {
    std::process::Command::new("date")
        .arg("+%Y-%m-%d")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn check(command: &str) -> i32 {
    let cfg = load_cfg_or_exit();
    let mode = ledger::active_mode(&cfg);
    let parsed = parse::parse(command);
    println!("mode:    {}", if mode.is_empty() { "(none)" } else { &mode });
    println!("parsed:  {} command(s)", parsed.commands.len());
    for c in &parsed.commands {
        println!(
            "         {:?}  ->  signature '{}'",
            c.argv,
            ledger::signature(&rules::normalise(&c.argv))
        );
    }
    if parsed.had_error {
        println!("         (parse error: the hook would ask)");
        return 0;
    }
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let verdicts = ledger::load_verdicts();
    let mut judge = |command: &str| agent::judge(&cfg.agent, &mode, command, &cwd);
    let v = rules::evaluate(&cfg, &verdicts, &parsed, &cwd, &mode, &mut judge);
    println!(
        "verdict: {}",
        match v.decision {
            Decision::Allow => "allow",
            Decision::Ask => "ask",
            Decision::Deny => "deny",
            Decision::Pass => "pass (Claude Code's own flow decides)",
        }
    );
    if let Some(r) = v.reason {
        println!("reason:  {r}");
    }
    if !v.unseen.is_empty() {
        println!(
            "queue:   would add {}",
            v.unseen.iter().map(|(s, _)| s.as_str()).collect::<Vec<_>>().join(", ")
        );
    }
    0
}
