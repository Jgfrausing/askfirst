# askfirst

A `PreToolUse` hook for Claude Code that learns what you allow.

It makes no attempt to understand what a command does. It reduces the call to a signature,
applies whatever you decided about that signature in the mode you are in, and asks about
anything it has never seen. What it asked about joins a queue you work through with
`askfirst review`.

The gate starts strict and loosens as you teach it, so nothing runs unattended until you have
said it may.

## Why this shape

Claude Code's permission rules match the command string, and its own documentation lists what
that misses: `Bash(git push *)` stops `git push origin main` and does not stop
`git -C . push origin main` or `git 'push' origin`. The docs say such a rule "isn't a security
boundary around the program".

The usual fix is a longer pattern list, which fails the same way: it only covers what you
thought of. On the machine this was written for, that gap was not hypothetical. There were 38
git aliases, and three of them pushed:

```
publish    !git push -u origin $(git rev-parse --abbrev-ref HEAD)
unpublish  !git push origin -d
cp         !f() { git commit -am "$*" && git push; }; f
```

Plus a shell alias `dot` for a bare dotfiles repo, and `gj`, a git front end. A `git push` rule
catches none of those five.

askfirst inverts it. It does not need to know that `git publish` pushes. It has not seen
`git publish`, so it stops and asks, and you tell it once what that is. Nothing has to be
discovered, and nothing is missed because it was not on a list.

## Install

```sh
cargo build --release
cp target/release/askfirst ~/.local/bin/
askfirst install
```

`install` writes the default config, a starter policy and the
`/askfirst` skill. It never overwrites a file that already exists. It then prints the hook
registration to add to `~/.claude/settings.json`:

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash|Edit|Write|MultiEdit|NotebookEdit",
        "hooks": [
          { "type": "command", "command": "/Users/you/.local/bin/askfirst", "args": [], "timeout": 45 }
        ]
      }
    ]
  }
}
```

The timeout must exceed `judge.timeout_secs`, or a judged call is killed mid-answer and the
tool call proceeds ungated.

`askfirst --help` lists the rest, and `askfirst completions <shell>` prints a completion script.
With no arguments at all it is the hook, reading an event on stdin.

## How a call is decided

Per sub-command, in order:

1. A rule in `rules.json`, in the mode you are in. What you decided during review is one of
   these: review writes its answers into the same file, in the same groups.
2. Never seen: whatever the mode's `unseen` says, except for askfirst's own commands, which
   ask.

The strictest outcome across every sub-command wins, so one unseen segment in
`ls && mystery-tool` stops the whole thing.

Only `askfirst review` writes a rule. The hook appends observations and nothing else, so a
session cannot widen its own permissions by running a command: running it is exactly what puts
it in the queue.

## Actions

| Action | Effect |
|---|---|
| `allow` | run without asking; in auto mode this also skips Claude Code's classifier |
| `ask` | prompt, every time |
| `deny` | refuse, and tell the model why |
| `pass` | say nothing and let Claude Code's own flow decide |
| `agent` | hand the whole command to a model, which answers allow/ask/deny against your policy |

Firmness, when several rules match: `allow < pass < agent < ask < deny`. Between two equally
firm matches, the more specific pattern is the one quoted back to you.

## Writing rules

Every rule belongs to a mode, grouped there by what it does:

```json
{
  "modes": {
    "contributor": {
      "unseen": "ask",
      "allow": ["cargo test *", "git status *"],
      "pass": ["ls *"],
      "ask": { "* push *": "Pushing publishes work. Confirm the remote and branch." },
      "deny": { "git push --force *": "Force push rewrites published history. Use --force-with-lease." },
      "agent": { "npx *": "npx runs a package off the network. Anything unpinned is ask." }
    }
  }
}
```

Nothing sits above the modes. A boundary you want everywhere is written into each mode that
should have it, which is a line repeated rather than a tier to reason about: the mode you are
in is the whole answer, and reading one mode tells you everything that mode does.

`allow` and `pass` are lists of patterns, because neither needs anything said about it. `ask`
and `deny` map a pattern to one sentence: it is what you are shown when the call stops, and
what the model is told about it. There is one string rather than a short reason and a longer
context, which said the same thing twice.

A pattern is matched word by word against the command. `*` matches one word, and a trailing
`*` matches any remaining words, so `git push *` covers a bare `git push`.

`agent` is for commands whose signature cannot tell you enough. `python3` says nothing about
what the script does, and `docker run` nothing about the image. Its value is an extra line of
policy for the judge, about that pattern alone, on top of the mode's entry in `policy.toml`.
Leave it empty for none:

```json
"agent": { "python3 *": "" }
```

Real answers from that one rule: `python3 -m json.tool data.json` allow,
`python3 deploy_to_prod.py --yes` deny.

## Modes

A mode is the posture you are working in. It holds its own rules and its own `unseen`, and
they are all that applies while it is active.

```
$ askfirst modes
* contributor   Edit, build, test and commit. Publishing and discarding still ask.
  reader        Read and investigate. Anything that changes state is refused.
  godmode       Allows everything. Built in; always available.
```

`askfirst mode <name>` switches for the current session, `--global` for every session,
`ASKFIRST_MODE` for one invocation. `askfirst defaultmode <name>` sets what new sessions start
in, and fails if the mode does not exist.

One queue, one pass. A signature waits once, whatever mode it came up in, and stays there
until every mode has an answer for it. `askfirst review` asks you once and writes the answer
into each mode that has none; `askfirst review --agent` asks each mode's policy separately, so
`reader` and `contributor` can come back with different answers for the same command. A mode
that already decides a signature is left alone either way.

`godmode` is decided in the binary rather than the config: it allows everything, overriding
every rule and `unseen` setting, and no rules file can redefine or remove it. It is
always available as a way out of a config that has locked itself up. Its one exception is
below.

A mode you have not defined has no rules, so `unseen` decides every call in it.

## Signatures

A signature is the program plus its first non-flag word, when that word is a plain identifier.
It is the granularity you review at:

| Command | Signature |
|---|---|
| `git push origin main` | `git push` |
| `git -C . push` | `git push` |
| `sudo git push` | `git push` |
| `/usr/bin/git push` | `git push` |
| `cargo test --release` | `cargo test` |
| `ls -la /tmp` | `ls` |

A path, a glob or a variable is an argument rather than a subcommand, so `ls /tmp` does not
become its own entry. During review, `b` broadens a signature to the program alone.

## Review

```
$ askfirst pending
6 signature(s) waiting:

  cargo build   (seen 2x, e.g. cargo build --release)
  git publish   (seen 1x, e.g. git publish)

$ askfirst review
6 signature(s) to categorise. Each answer is written into ~/.config/askfirst/rules.json
as a rule, in every mode that has no rule for it yet: contributor, reader.

[1/6] cargo build
          seen 2x, e.g. cargo build --release
          a/s/d/p/j/b/k/q > a
          cargo build * -> allow in contributor, reader
```

`a` allow, `s` ask every time, `d` deny, `p` pass, `j` judge every time, `b` broaden to the
program, `k` skip, `q` quit and save.

`askfirst review --agent` resolves the queue with your policy instead, one question per mode
per signature, writing only a confident allow or deny. Anything a policy does not settle stays
in the queue for you:

```
$ askfirst review --agent
  curl                         reader: deny, contributor: deny
  rg                           reader: allow, contributor: allow
  cargo build                  reader: deny, contributor: allow
  some-weird-tool              reader: left (not settled by the policy), contributor: left

6 rule(s) written to ~/.config/askfirst/rules.json, 1 signature(s) left for you.
```

That writes rules from a model's answers, which the hook itself never does. It is safe only
because it is one deliberate command you run, not something a session can reach.

## The policy

`policy.toml` is the prompt the `agent` action and `review --agent` judge against, plus
whatever line the matching `agent` rule carries. One entry
per mode, in plain English, plus a `shared` block prepended to all of them. It stays TOML while
everything else is JSON because it is the one file that is mostly prose, and TOML has a
multi-line string.

```toml
shared = """
Anything that sends the contents of this machine somewhere else is a deny.
"""

[modes]
contributor = """
Agents should be allowed to pull, commit, and edit files. Discarding changes
and pushing should be prompted first.
...
"""
```

A judge that cannot tell from the policy answers `ask`, so an incomplete policy is safe and a
vague one is not. Every failure is `ask` too: no policy file, no entry for the mode, a timeout,
a non-zero exit, a hedged answer, a recursive call.

The judge defaults to `claude -p --model haiku` using your existing login, with hooks disabled,
MCP loading skipped and all tools disallowed. Set `judge.model` to change it, or
`judge.command` and `judge.args` for a judge that is not the Claude CLI. The block is called
`judge` because `agent` is the name of an action. A judged call costs
6 to 8 seconds, which is why `unseen = "agent"` is a poor fit for interactive work and
`review --agent` is the better use of it.

## Protecting itself

A gate whose rules the agent can rewrite is decoration, so askfirst always asks before anything
touches:

- its own config directory, and the default `~/.config/askfirst` even when `ASKFIRST_HOME`
  points elsewhere
- its own binary
- `~/.claude/settings.json` and `settings.local.json`, where the hook is registered and where
  an `env` block could redirect `ASKFIRST_HOME`
- any invocation of askfirst itself, except `askfirst check`, unless you wrote a rule naming
  that command

This holds through a shell (`sed -i`, a `>` redirect, `mv`, `rm`, `tee`), through the file
tools (`Edit`, `Write`, `MultiEdit`, `NotebookEdit`), through wrappers (`sudo`, `env`,
`bash -c`), and in `godmode`. That last exception is what keeps godmode reversible: entering it
costs one approval, and if it also bypassed this, that single approval could be made permanent.

Running askfirst is the one part of this a rule can decide, and the rule has to name askfirst:

```json
"modes": { "reader": { "allow": ["askfirst mode *"] } }
```

A catch-all such as `"allow": ["*"]` is written about other commands and does not decide this
one. With no such rule, running askfirst asks, whatever the mode's
`unseen` says. That is what lets a session ask to leave a mode it cannot work in: `reader` sets
`unseen = "deny"`, and taking that answer would refuse `askfirst mode contributor`, the only
command that leaves `reader`, without ever offering you the prompt. A mode's `unseen` never
decides the command that leaves it.

Naming a file is not the same as writing to it, and telling them apart needs the semantics of
every program, so this errs toward asking.

## Files

| Path | Written by | Holds |
|---|---|---|
| `~/.config/askfirst/rules.json` | you, and `askfirst review` | rules, modes, judge config |
| `~/.config/askfirst/policy.toml` | you | the prose the judge reads |
| `~/.config/askfirst/pending.jsonl` | the hook | signatures seen with no decision yet |
| `~/.config/askfirst/state.json` | askfirst | global mode, current session |
| `~/.config/askfirst/sessions.json` | askfirst | per-session modes |

`ASKFIRST_HOME` moves the directory; it cannot unprotect the default one.

The queue is JSON Lines rather than a JSON array because it is appended to by concurrent hook
processes, and an array cannot be appended without a rewrite that would lose entries.

## What it handles

Written as tests in `src/parse.rs`, `src/ledger.rs`, `src/rules.rs` and `src/selfguard.rs`:

- operators `&&`, `||`, `;`, `|`, and nesting through `$(...)`, backticks, `(...)`, `if` and
  `for` bodies
- `bash -c "..."` and `sh -lc '...'`, re-parsed rather than treated as one opaque argument
- quotes, so `git 'push'` reads as `git push`
- leading assignments, so `FOO=bar git push` still finds git
- wrappers: `sudo`, `env`, `nohup`, `time`, `timeout`, `nice`, `xargs`, `command`
- global flags that hide a subcommand: `git -C`, `git -c`, `--git-dir=`, and the equivalents
  for `docker` and `kubectl`

About 2.7 ms per call including process spawn, when no judge is involved.

## What it does not do

Bash and the file tools only. Every other tool passes through untouched.

Signatures ignore arguments. A rule on `rm *` covers `rm -rf /` as well as `rm foo`. Where the
arguments are the danger, write a rule or use `action = "agent"`.

It cannot see inside a script. `bash deploy.sh` is judged on the name `bash`; what the file
contains is invisible.

The judge reads a command another model wrote. It is fenced and labelled as data with an
explicit instruction not to follow it, which narrows prompt injection rather than closing it.
Keep the boundaries you actually care about in `rules.json`, where no judge is consulted.

## Failure behaviour

Failure escalates to `ask`, never to silence: an unreadable or malformed rules file, a command
tree-sitter cannot parse, unreadable hook input, a mode that is set but not defined. A gate that
fails open is worse than no gate, because it looks like it is working. The one exception is
input that is not a hook event, where it stays silent rather than blocking a session on its own
bug.

Keep a `permissions.ask` rule underneath it for the boundaries that matter. Claude Code
"evaluates deny and ask rules regardless of what a PreToolUse hook returns", so that rule still
fires on a call this binary never got to judge.

## Development

`rules.default.toml` is the source of truth for the shipped defaults. After editing it:

```sh
python3 embed-defaults.py   # regenerates rules.default.json and bakes the defaults into main.rs
cargo test
```

The defaults live in the binary so `askfirst install` works on a fresh machine.
