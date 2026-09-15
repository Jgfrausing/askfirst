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

`install` writes the default config, a starter policy, a seed of read-only verdicts and the
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

The timeout must exceed `agent.timeout_secs`, or a judged call is killed mid-answer and the
tool call proceeds ungated.

## How a call is decided

Per sub-command, in order:

1. A rule you wrote in `rules.json`. Shared rules plus the active mode's.
2. A verdict you gave during review, for the signature or for the program as a whole.
3. Never seen: whatever the mode's `unseen` says.

The strictest outcome across every sub-command wins, so one unseen segment in
`ls && mystery-tool` stops the whole thing.

Only `askfirst review` writes a verdict. The hook appends observations and nothing else, so a
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

Firmness, when several rules match: `allow < pass < agent < ask < deny`.

`agent` is for commands whose signature cannot tell you enough. `python3` says nothing about
what the script does, and `docker run` nothing about the image:

```json
{ "match": "python3 *", "action": "agent" }
```

Real verdicts from that one rule: `python3 -m json.tool data.json` allow,
`python3 deploy_to_prod.py --yes` deny.

## Modes

A mode is the posture you are working in. Rules written at the top level apply in every mode;
a mode's own rules are added to them, so a mode can tighten a shared rule but never loosen one.
Each mode sets its own `unseen`.

```
$ askfirst modes
* contributor   Edit, build, test and commit. Publishing and discarding still ask.
  reader        Read and investigate. Anything that changes state is refused.
  godmode       Allows everything. Built in; always available.
```

`askfirst mode <name>` switches for the current session, `--global` for every session,
`ASKFIRST_MODE` for one invocation. `askfirst defaultmode <name>` sets what new sessions start
in, and fails if the mode does not exist.

Verdicts are scoped to the mode you gave them in, so what you allow while contributing does not
follow you into a read-only session. During review, `g` records a decision for every mode.

`godmode` is decided in the binary rather than the config: it allows everything, overriding
every rule, verdict and `unseen` setting, and no rules file can redefine or remove it. It is
always available as a way out of a config that has locked itself up. Its one exception is
below.

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
6 signature(s) waiting in mode 'contributor':

  cargo build   (seen 2x, e.g. cargo build --release)
  git publish   (seen 1x, e.g. git publish)

$ askfirst review
[1/6] cargo build
          seen 2x, e.g. cargo build --release
          a/s/d/p/j/b/g/k/q > a
          cargo build -> allow
```

`a` allow, `s` ask every time, `d` deny, `p` pass, `j` judge every time, `b` broaden to the
program, `g` apply to every mode, `k` skip, `q` quit and save.

`askfirst review --agent` resolves the queue with your policy instead, recording only a
confident allow or deny and leaving everything else for you:

```
$ askfirst review --agent
  curl                         deny
  rg src                       allow
  some-weird-tool              left  (the contributor policy judged this 'ask')

2 decided by the policy, 1 left for you.
```

That writes verdicts from a model's answers, which the hook itself never does. It is safe only
because it is one deliberate command you run, not something a session can reach.

## The policy

`policy.toml` is the prompt the `agent` action and `review --agent` judge against. One entry
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
MCP loading skipped and all tools disallowed. Set `agent.model` to change it, or
`agent.command` and `agent.args` for a judge that is not the Claude CLI. A judged call costs
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
- any invocation of askfirst itself, except `askfirst check`

This holds through a shell (`sed -i`, a `>` redirect, `mv`, `rm`, `tee`), through the file
tools (`Edit`, `Write`, `MultiEdit`, `NotebookEdit`), through wrappers (`sudo`, `env`,
`bash -c`), and in `godmode`. That last exception is what keeps godmode reversible: entering it
costs one approval, and if it also bypassed this, that single approval could be made permanent.

Naming a file is not the same as writing to it, and telling them apart needs the semantics of
every program, so this errs toward asking.

## Files

| Path | Written by | Holds |
|---|---|---|
| `~/.config/askfirst/rules.json` | you | rules, modes, judge config |
| `~/.config/askfirst/policy.toml` | you | the prose the judge reads |
| `~/.config/askfirst/verdicts.json` | `askfirst review` | what you decided, per mode |
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

Signatures ignore arguments. A verdict on `rm` covers `rm -rf /` as well as `rm foo`. Where the
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
