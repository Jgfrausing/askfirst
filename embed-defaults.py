#!/usr/bin/env python3
"""Bake the default config files into src/main.rs.

`askfirst install` writes these, so they have to live in the binary. Keeping
them as files in the repo and embedding them here means one source of truth:
edit rules.default.toml, run this, rebuild.

rules.default.toml is the source for rules.default.json, so the two cannot
disagree about what the defaults are.
"""
import json
import re
import sys
import tomllib
from pathlib import Path

root = Path(__file__).parent


def build_rules_json() -> str:
    d = tomllib.load((root / "rules.default.toml").open("rb"))
    out = {
        "//": "askfirst rules. Keys starting with // are comments; askfirst ignores them.",
        "//actions": "allow | ask | deny | pass | agent. Firmness: allow < pass < agent < ask < deny.",
        "//unseen": "What happens to a signature with no rule and no verdict: ask (queue it, instant), agent (judge it, 6-8s), deny, or pass.",
        "default_mode": d["default_mode"],
        "unseen": d["unseen"],
        "agent": {
            "//": "The judge for action=agent, unseen=agent and `askfirst review --agent`.",
            "//timeout": "Keep the PreToolUse hook timeout in settings.json comfortably above this.",
            **d["agent"],
        },
        "//rules": "Apply in every mode. A mode may tighten these, never loosen them.",
        "rules": d["rules"],
        "modes": {name: dict(m) for name, m in d["modes"].items()},
    }
    if "godmode" in out["modes"]:
        out["modes"]["godmode"]["//"] = (
            "A catch-all allow. The shared rules still apply (a push still asks, a force "
            "push is still denied) and the self-guard is compiled in, so askfirst's own "
            "config still asks."
        )
    return json.dumps(out, indent=2) + "\n"


def replace_const(src: str, name: str, body: str) -> str:
    """Swap the body of `const NAME: &str = r##"..."##;`."""
    pattern = re.compile(
        r'(const ' + re.escape(name) + r': &str = r##")(.*?)("##;)', re.S
    )
    if not pattern.search(src):
        sys.exit(f"could not find const {name} in src/main.rs")
    if '"##' in body:
        sys.exit(f"{name} contains the raw-string terminator; pick a longer delimiter")
    return pattern.sub(lambda m: m.group(1) + body + m.group(3), src, count=1)


rules = build_rules_json()
(root / "rules.default.json").write_text(rules)

policy = (root / "policy.example.toml").read_text()
verdicts = (root / "verdicts.seed.json").read_text()

main = (root / "src" / "main.rs").read_text()
main = replace_const(main, "DEFAULT_RULES", rules)
main = replace_const(main, "DEFAULT_POLICY", policy)
main = replace_const(main, "SEED_VERDICTS", verdicts)
(root / "src" / "main.rs").write_text(main)

d = json.loads(rules)
print(f"embedded: {len(d['rules'])} shared rules, modes {list(d['modes'])}")
print(f"          policy {len(policy)} bytes, seed {len(json.loads(verdicts)['modes']['*']['entries'])} verdicts")
