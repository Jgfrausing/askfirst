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

    def groups(src: dict) -> dict:
        """The five action groups, in firmness order, skipping the empty ones."""
        out = {}
        for k in ("allow", "pass", "agent", "ask", "deny"):
            v = src.get(k)
            if v:
                out[k] = v
        return out

    out = {
        "//": "askfirst rules. Keys starting with // are comments; askfirst ignores them.",
        "//rules": "Every rule belongs to a mode, grouped by action there. allow and pass are lists of patterns; ask and deny map a pattern to the sentence you are shown and the model is told; agent maps a pattern to an extra prompt for the judge (empty for none).",
        "//firmness": "When several match: allow < pass < agent < ask < deny, then the more specific pattern.",
        "//unseen": "What happens to a signature with no rule and no verdict: ask (queue it, instant), agent (judge it, 6-8s), deny, or pass.",
        "//askfirst": "askfirst's own commands ignore unseen and ask, in every mode, unless a rule names askfirst (a catch-all match does not count). Otherwise a mode with unseen=deny would refuse `askfirst mode <name>`, the only command that leaves it, without prompting.",
        "default_mode": d["default_mode"],
        "unseen": d["unseen"],
        "judge": {
            "//": "The model behind an `agent` rule, `unseen = agent` and `askfirst review --agent`.",
            "//timeout": "Keep the PreToolUse hook timeout in settings.json comfortably above this.",
            **d["judge"],
        },
        "modes": {
            name: {
                **{k: v for k, v in m.items() if k in ("description", "unseen")},
                **groups(m),
            }
            for name, m in d["modes"].items()
        },
    }
    if "godmode" in out["modes"]:
        out["modes"]["godmode"]["//"] = (
            "Its meaning is fixed in the binary: it allows everything, and this entry "
            "supplies only the description. The self-guard is compiled in, so askfirst's "
            "own config still asks."
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

main = (root / "src" / "main.rs").read_text()
main = replace_const(main, "DEFAULT_RULES", rules)
main = replace_const(main, "DEFAULT_POLICY", policy)
(root / "src" / "main.rs").write_text(main)

d = json.loads(rules)
counts = {
    name: sum(len(m.get(k, [])) for k in ("allow", "pass", "agent", "ask", "deny"))
    for name, m in d["modes"].items()
}
print(f"embedded: rules per mode {counts}")
print(f"          policy {len(policy)} bytes")
