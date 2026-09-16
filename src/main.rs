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

use clap::{CommandFactory as _, Parser};
use clap_complete_command::Shell;
use config::Action;

/// Tools that write a file directly. They never reach a shell, so the command
/// parsing does not apply; only their path matters.
const FILE_TOOLS: &[&str] = &["Edit", "Write", "MultiEdit", "NotebookEdit"];
use hook::Decision;
use std::io::{BufRead, Read, Write};
use std::path::PathBuf;

/// The command line.
///
/// With no arguments at all askfirst is the hook: it reads a Claude Code event
/// on stdin and writes a decision on stdout. Everything below is for you.
#[derive(Parser, Debug)]
#[command(
    name = "askfirst",
    version,
    about = "A PreToolUse hook for Claude Code that learns what you allow",
    long_about = "A PreToolUse hook for Claude Code that learns what you allow.\n\n\
                  With no arguments it reads a hook event on stdin and writes a decision on \
                  stdout. A command it has not seen asks, and joins the review queue. What you \
                  decide during review is written into the rules file as a rule.\n\n\
                  Everything lives in one directory; `askfirst paths` prints it. ASKFIRST_HOME \
                  moves that directory, ASKFIRST_MODE overrides the mode.",
    disable_help_subcommand = true
)]
enum Cli {
    /// Print the mode in force, or switch to one
    Mode {
        /// The mode to switch to. Omit to print the mode in force
        #[arg(value_name = "NAME")]
        name: Option<String>,

        /// Switch for every session, not just this one
        #[arg(short, long)]
        global: bool,

        /// Create the mode first, with `unseen = "ask"` and no rules
        #[arg(short, long)]
        create: bool,
    },

    /// Set the mode new sessions start in
    #[command(name = "defaultmode")]
    DefaultMode {
        #[arg(value_name = "NAME")]
        name: String,
    },

    /// Write the default rules, the starter policy and the /askfirst skill
    Install,

    /// List the modes
    Modes,

    /// Categorise what is waiting, writing a rule for each answer
    Review {
        /// Let each mode's policy judge the queue instead of asking you
        #[arg(short, long)]
        agent: bool,

        /// Removed: the queue is not per mode
        #[arg(long, hide = true, value_name = "MODE")]
        mode: Option<String>,
    },

    /// List what is waiting, without deciding anything
    Pending,

    /// Explain what would happen to one command, changing nothing
    Check {
        #[arg(value_name = "COMMAND")]
        command: String,
    },

    /// Print the files askfirst uses
    Paths,

    /// Print a shell completion script
    Completions {
        #[arg(value_enum)]
        shell: Shell,
    },
}

fn main() {
    // Rust ignores SIGPIPE, so `askfirst pending | head` panics instead of
    // ending quietly. Restore the default so this behaves like other CLIs.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    // The hook call carries no arguments, and it is the hot path: every tool
    // call in every session goes through it, so it never builds the parser.
    if std::env::args().nth(1).is_none() {
        run_hook();
        return;
    }

    let code = match Cli::parse() {
        Cli::Mode { name, global, create } => mode_cmd(name.as_deref(), global, create),
        Cli::DefaultMode { name } => default_mode_cmd(Some(&name)),
        Cli::Install => install_cmd(),
        Cli::Modes => modes_cmd(),
        Cli::Review { agent, mode } => {
            if let Some(m) = mode {
                // It used to select which mode's queue to work through. What
                // you decide now goes to every mode that has no rule for it,
                // so there is nothing left to select.
                eprintln!(
                    "the review queue is not per mode: what you decide is written into every \
                     mode that has no rule of its own. Drop `--mode {m}`."
                );
                2
            } else if agent {
                review_with_agent()
            } else {
                review()
            }
        }
        Cli::Pending => show_pending(),
        Cli::Check { command } => check(&command),
        Cli::Paths => {
            println!("dir:      {}", config::dir().display());
            println!("rules:    {}", config::default_path().display());
            println!("pending:  {}", ledger::pending_path().display());
            println!("state:    {}", ledger::state_path().display());
            println!("policy:   {}", agent::policy_path().display());
            println!("sessions: {}", ledger::sessions_path().display());
            0
        }
        Cli::Completions { shell } => {
            shell.generate(&mut Cli::command(), &mut std::io::stdout());
            0
        }
    };
    std::process::exit(code);
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
    // A mode that is set but not defined has no rules at all, so every call
    // would fall to `unseen` in a mode you thought you had written.
    if !mode.is_empty() && !cfg.knows_mode(&mode) {
        return escalate(&format!(
            "askfirst is set to mode '{mode}', which is not defined in its rules"
        ));
    }

    let mut judge =
        |command: &str, extra: Option<&str>| agent::judge(&cfg.judge, &mode, command, &input.cwd, extra);
    let v = rules::evaluate(&cfg, &parsed, &input.cwd, &mode, &mut judge);

    // Record what we had no decision for. This is an observation, never a
    // permission: only `askfirst review` writes a verdict.
    for (sig, example) in &v.unseen {
        ledger::record_sighting(sig, example);
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

/// Is this running inside a Claude Code session?
///
/// Claude Code sets CLAUDECODE in the environment of every command it runs.
/// Outside one there is no session to report on or switch, so the session
/// paths of `askfirst mode` say nothing rather than answering about whichever
/// session the hook happened to serve last.
fn in_session() -> bool {
    std::env::var("CLAUDECODE").is_ok_and(|v| !v.trim().is_empty())
}

fn mode_cmd(name: Option<&str>, global: bool, create: bool) -> i32 {
    // `--global` is not session-scoped, so it still works from a shell.
    if !in_session() && !global {
        return 0;
    }
    let cfg = load_cfg_or_exit();
    let Some(name) = name else {
        let current = ledger::active_mode(&cfg);
        if current.is_empty() {
            println!("no mode set; there are no rules outside a mode, so `unseen` decides everything");
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

    let waiting = ledger::pending(&cfg).len();
    if waiting > 0 {
        println!("{waiting} signature(s) waiting; `askfirst review` to categorise");
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
  "//rules": "Every rule belongs to a mode, grouped by action there. allow and pass are lists of patterns; ask and deny map a pattern to the sentence you are shown and the model is told; agent maps a pattern to an extra prompt for the judge (empty for none).",
  "//firmness": "When several match: allow < pass < agent < ask < deny, then the more specific pattern.",
  "//unseen": "What happens to a signature with no rule and no verdict: ask (queue it, instant), agent (judge it, 6-8s), deny, or pass.",
  "//askfirst": "askfirst's own commands ignore unseen and ask, in every mode, unless a rule names askfirst (a catch-all match does not count). Otherwise a mode with unseen=deny would refuse `askfirst mode <name>`, the only command that leaves it, without prompting.",
  "default_mode": "contributor",
  "unseen": "ask",
  "judge": {
    "//": "The model behind an `agent` rule, `unseen = agent` and `askfirst review --agent`.",
    "//timeout": "Keep the PreToolUse hook timeout in settings.json comfortably above this.",
    "model": "haiku",
    "timeout_secs": 20
  },
  "modes": {
    "reader": {
      "description": "Read and investigate. Anything that changes state is refused.",
      "unseen": "deny",
      "allow": [
        "git status *",
        "git log *",
        "git diff *",
        "git show *",
        "git blame *",
        "cargo tree *",
        "cargo metadata *",
        "basename *",
        "cat *",
        "cmp *",
        "column *",
        "cut *",
        "date *",
        "df *",
        "diff *",
        "dirname *",
        "du *",
        "echo *",
        "false *",
        "file *",
        "grep *",
        "groups *",
        "head *",
        "hostname *",
        "id *",
        "jq *",
        "less *",
        "locale *",
        "ls *",
        "man *",
        "md5 *",
        "printf *",
        "pwd *",
        "realpath *",
        "rg *",
        "shasum *",
        "sleep *",
        "sort *",
        "stat *",
        "tail *",
        "tput *",
        "tr *",
        "tree *",
        "true *",
        "type *",
        "uname *",
        "uniq *",
        "uptime *",
        "wc *",
        "which *",
        "whoami *"
      ],
      "deny": {
        "git commit *": "reader mode does not commit. It investigates without changing anything: report what you found, and let the user switch to contributor mode if they want the change made.",
        "git add *": "reader mode does not stage changes. Describe the change you would make instead.",
        "* push *": "reader mode never publishes. Do not push; tell the user what you would have pushed."
      }
    },
    "contributor": {
      "description": "Edit, build, test and commit. Publishing and discarding still ask.",
      "unseen": "ask",
      "allow": [
        "cargo build *",
        "cargo test *",
        "cargo check *",
        "cargo clippy *",
        "cargo fmt *",
        "git add *",
        "git commit *",
        "git checkout *",
        "git switch *",
        "git fetch *",
        "git pull *",
        "basename *",
        "cat *",
        "cmp *",
        "column *",
        "cut *",
        "date *",
        "df *",
        "diff *",
        "dirname *",
        "du *",
        "echo *",
        "false *",
        "file *",
        "grep *",
        "groups *",
        "head *",
        "hostname *",
        "id *",
        "jq *",
        "less *",
        "locale *",
        "ls *",
        "man *",
        "md5 *",
        "printf *",
        "pwd *",
        "realpath *",
        "rg *",
        "shasum *",
        "sleep *",
        "sort *",
        "stat *",
        "tail *",
        "tput *",
        "tr *",
        "tree *",
        "true *",
        "type *",
        "uname *",
        "uniq *",
        "uptime *",
        "wc *",
        "which *",
        "whoami *"
      ],
      "agent": {
        "python3 *": "",
        "python *": "",
        "node *": "",
        "npx *": "npx fetches a package from the network and runs it. Allow a pinned, well-known package that only builds or formats; anything unfamiliar or unpinned is ask.",
        "uvx *": "uvx fetches a package from the network and runs it. Allow a pinned, well-known package that only builds or formats; anything unfamiliar or unpinned is ask."
      },
      "ask": {
        "* push *": "Pushing publishes work. Confirm the remote and branch with the user, whatever the program or alias. Wait for their answer; do not retry through a script, a wrapper or a different spelling.",
        "git reset --hard *": "This discards uncommitted work. Confirm with the user what would be lost before running it.",
        "git clean *": "This deletes untracked files.",
        "git stash *": "Stashing can hide uncommitted work."
      },
      "deny": {
        "git push --force *": "Force push rewrites published history, so it is refused. Use --force-with-lease and ask the user first, or rebase locally and push normally.",
        "git push -f *": "Force push rewrites published history, so it is refused. Use --force-with-lease and ask the user first, or rebase locally and push normally."
      }
    },
    "godmode": {
      "description": "Everything runs. The self-guard still applies.",
      "unseen": "pass",
      "//": "Its meaning is fixed in the binary: it allows everything, and this entry supplies only the description. The self-guard is compiled in, so askfirst's own config still asks."
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
    for (name, def) in &cfg.modes {
        let marker = if *name == current { "*" } else { " " };
        let desc = def.description.clone().unwrap_or_default();
        println!("{marker} {name:<14}{desc}");
    }
    if !cfg.modes.contains_key(config::GODMODE) {
        let marker = if current == config::GODMODE { "*" } else { " " };
        println!(
            "{marker} {:<14}Allows everything. Built in; always available.",
            config::GODMODE
        );
    }
    let waiting = ledger::pending(&cfg).len();
    if waiting > 0 {
        println!(
            "\n{waiting} signature(s) waiting; `askfirst review` writes an answer into every mode"
        );
    }
    0
}

fn show_pending() -> i32 {
    let cfg = load_cfg_or_exit();
    let items = ledger::pending(&cfg);
    if items.is_empty() {
        println!("Nothing waiting.");
        return 0;
    }
    println!("{} signature(s) waiting:\n", items.len());
    for (sig, example, n) in &items {
        println!("  {sig}   (seen {n}x, e.g. {example})");
    }
    println!("\nRun `askfirst review` to categorise them.");
    0
}

/// Walk the queue and write down what you decide.
///
/// This is the only thing that writes a verdict. The hook observes; you decide.
fn review() -> i32 {
    let cfg = load_cfg_or_exit();
    let items = ledger::pending(&cfg);
    if items.is_empty() {
        return show_pending();
    }

    // Every mode you have written, in the order `askfirst modes` lists them.
    // godmode reads no rules, so there is nothing to write there.
    let modes: Vec<String> = cfg
        .modes
        .keys()
        .filter(|m| *m != config::GODMODE)
        .cloned()
        .collect();
    if modes.is_empty() {
        eprintln!(
            "no modes defined in {}, so there is nowhere to write a decision.",
            config::default_path().display()
        );
        eprintln!("Create one with `askfirst mode <name> --create`.");
        return 2;
    }

    let mut decisions: Vec<config::Decided> = Vec::new();
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    let total = items.len();

    println!(
        "{total} signature(s) to categorise. Each answer is written into {} as a rule,\n\
         in every mode that has no rule for it yet: {}.\n\
         a allow, s ask each time, d deny, p pass to Claude Code, j judge each time\n\
         b broaden to the program, k skip, q quit and save\n",
        config::default_path().display(),
        modes.join(", ")
    );

    for (i, (sig, example, n)) in items.iter().enumerate() {
        let mut target = sig.clone();
        loop {
            println!("[{}/{total}] {target}", i + 1);
            println!("          seen {n}x, e.g. {example}");
            print!("          a/s/d/p/j/b/k/q > ");
            let _ = std::io::stdout().flush();

            let Some(Ok(line)) = lines.next() else {
                println!("\nno more input, saving what you decided");
                return finish(&decisions);
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
                "k" => break,
                "q" => return finish(&decisions),
                other => {
                    println!("          '{other}'? use a, s, d, p, j, b, k or q");
                    continue;
                }
            };
            // A signature is not a pattern: `cargo test` has to become
            // `cargo test *` to cover the arguments that follow it.
            let pattern = format!("{target} *");
            let mut written: Vec<&str> = Vec::new();
            for m in &modes {
                if cfg.covers(m, &target) {
                    continue; // this mode already answers for it
                }
                decisions.push(config::Decided {
                    mode: m.clone(),
                    action,
                    pattern: pattern.clone(),
                    note: None,
                });
                written.push(m);
            }
            if written.is_empty() {
                println!("          every mode already decides {target}\n");
            } else {
                println!(
                    "          {pattern} -> {} in {}\n",
                    action.label(),
                    written.join(", ")
                );
            }
            break;
        }
    }
    finish(&decisions)
}

/// Resolve the queue with the judge, against every mode's policy at once.
///
/// Only a confident `allow` or `deny` is written. Anything the policy does not
/// settle, and anything that fails, stays in the queue and is listed at the
/// end, because the point of the queue is that you decide what the policy did
/// not. This writes verdicts from a model's answers, which the hook itself
/// never does: it is one deliberate command, not something a session can do.
fn review_with_agent() -> i32 {
    let cfg = load_cfg_or_exit();
    let items = ledger::pending(&cfg);
    if items.is_empty() {
        return show_pending();
    }

    // One pass per mode, against that mode's own policy. The modes are the
    // point: `reader` and `contributor` should not be given the same answer
    // about `cargo build`, and a judge asked for one answer covering both can
    // only give the stricter one. godmode reads no rules, so it is skipped,
    // and a mode with no policy entry has nothing to judge against.
    let policy = match agent::load_policy() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let modes: Vec<String> = cfg
        .modes
        .keys()
        .filter(|m| *m != config::GODMODE)
        .filter(|m| agent::policy_for(&policy, m).is_some())
        .cloned()
        .collect();
    if modes.is_empty() {
        eprintln!(
            "{} has no entry for any mode in {}, so the judge has nothing to judge against.",
            agent::policy_path().display(),
            config::default_path().display()
        );
        return 2;
    }

    println!(
        "Judging {} signature(s) in {} mode(s) ({}) against {}, using {}.\n",
        items.len(),
        modes.len(),
        modes.join(", "),
        agent::policy_path().display(),
        cfg.judge.model
    );

    let mut decisions: Vec<config::Decided> = Vec::new();
    let mut left: Vec<(String, String)> = Vec::new();

    for (sig, example, n) in &items {
        let pattern = format!("{sig} *");
        let mut answers: Vec<String> = Vec::new();
        let mut undecided: Vec<String> = Vec::new();
        for m in &modes {
            if cfg.covers(m, sig) {
                answers.push(format!("{m}: already decided"));
                continue;
            }
            let (d, why) = agent::judge(&cfg.judge, m, example, "", None);
            let action = match d {
                Decision::Allow => Some(Action::Allow),
                Decision::Deny => Some(Action::Deny),
                // Ask is the judge declining to decide, which is the answer it
                // is told to give when unsure. Leave it for a person.
                _ => None,
            };
            match action {
                Some(a) => {
                    decisions.push(config::Decided {
                        mode: m.clone(),
                        action: a,
                        pattern: pattern.clone(),
                        note: Some(format!(
                            "{m} policy, judged by `askfirst review --agent` on {}",
                            today()
                        )),
                    });
                    answers.push(format!("{m}: {}", a.label()));
                }
                None => {
                    answers.push(format!(
                        "{m}: left ({})",
                        why.unwrap_or_else(|| "not settled by the policy".into())
                    ));
                    undecided.push(m.clone());
                }
            }
        }
        println!("  {:<28} {}", sig, answers.join(", "));
        if !undecided.is_empty() {
            left.push((
                sig.clone(),
                format!("seen {n}x in {}, e.g. {example}", undecided.join(", ")),
            ));
        }
    }

    let wrote = decisions.len();
    if let Err(e) = config::add_rules(&decisions) {
        eprintln!("could not write the rules: {e}");
        return 1;
    }
    let cfg = config::load(&config::default_path()).unwrap_or(cfg);
    if let Err(e) = ledger::prune_pending(&cfg) {
        eprintln!("could not prune the queue: {e}");
    }

    println!(
        "\n{wrote} rule(s) written to {}, {} signature(s) left for you.",
        config::default_path().display(),
        left.len()
    );
    if !left.is_empty() {
        println!("\nStill waiting:");
        for (sig, detail) in &left {
            println!("  {sig}   ({detail})");
        }
        println!("\nRun `askfirst review` to work through them.");
    }
    0
}

fn finish(decisions: &[config::Decided]) -> i32 {
    if decisions.is_empty() {
        println!("nothing decided, nothing written");
        return 0;
    }
    if let Err(e) = config::add_rules(decisions) {
        eprintln!("could not write the rules: {e}");
        return 1;
    }
    let cfg = config::load(&config::default_path()).unwrap_or_default();
    if let Err(e) = ledger::prune_pending(&cfg) {
        eprintln!("could not prune the queue: {e}");
    }
    println!(
        "{} rule(s) written to {}",
        decisions.len(),
        config::default_path().display()
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
    let mut judge =
        |command: &str, extra: Option<&str>| agent::judge(&cfg.judge, &mode, command, &cwd, extra);
    let v = rules::evaluate(&cfg, &parsed, &cwd, &mode, &mut judge);
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
