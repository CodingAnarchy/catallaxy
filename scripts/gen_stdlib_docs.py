#!/usr/bin/env python3
"""Generates docs/stdlib.md from the prelude sources (the single source of truth for the API)."""
import re, pathlib

root = pathlib.Path(__file__).resolve().parent.parent
prelude = root / "crates/catallaxy-compiler/src/prelude"
out = ["# Catallaxy Standard Library", "",
       "Generated from the prelude by `scripts/gen_stdlib_docs.py`; do not edit by hand.",
       "Everything here is available in every module without an import.", ""]

def is_internal(line):
    # Functions starting with `_` are implementation details (e.g. `__fiber_spawn`, `Deque._grow`).
    return re.match(r"(?:pub )?(?:native )?fn _", line.strip()) is not None

def sig_of(line):
    line = line.strip()
    line = re.sub(r"\s*\{.*$", "", line) if "{" in line and not line.startswith("{") else line
    return line.replace("native fn ", "fn ").replace("pub fn ", "fn ").replace("pub native fn ", "fn ")

for f in ["core.cx", "builtins.cx"]:
    lines = (prelude / f).read_text().splitlines()
    depth = 0
    section = None
    pending_doc = []
    for raw in lines:
        line = raw.strip()
        if depth == 0:
            m = re.match(r"(?:pub )?(?:(frozen|shared) )?(interface|class|enum|extend(?:\[[^\]]*\])?)\s+([^{]+?)\s*\{", line)
            if m:
                modifier, kind, name = m.group(1), m.group(2), m.group(3).strip()
                title = {"interface": "interface", "class": "", "enum": "enum"}.get(kind, "")
                head = f"{title} {name}".strip() if not kind.startswith("extend") else name
                if modifier:
                    head += f" ({modifier})"
                if kind.startswith("extend") and "[" in kind:
                    bounded = [p.strip() for p in kind[7:-1].split(",") if ":" in p]
                    head = f"{name} (where {', '.join(bounded)})"
                out += ["", f"## {head}", ""]
                section = head
            elif line.startswith(("pub native fn", "pub fn", "native fn", "fn")) and not is_internal(line):
                if section != "Global functions":
                    out += ["", "## Global functions", ""]
                    section = "Global functions"
                out.append(f"- `{sig_of(line)}`")
        elif depth == 1 and section:
            if line.startswith(("fn ", "native fn ", "pub fn ", "mut fn")):
                if not is_internal(line):
                    out.append(f"- `{sig_of(line)}`")
            elif re.match(r"^[a-z]\w*: ", line):
                # Fields starting with `_` are implementation details (e.g. `Fiber._result`).
                out.append(f"- field `{line}`")
            elif line.startswith("const "):
                out.append(f"- `{line}`")
            elif re.match(r"^[A-Z]\w*(\(.*\))?$", line):
                out.append(f"- variant `{line}`")
        depth += raw.count("{") - raw.count("}")
        # Strings containing braces are rare in the prelude; interpolation braces balance.

(root / "docs/stdlib.md").write_text("\n".join(out).replace("\n\n\n", "\n\n") + "\n")
print(f"wrote docs/stdlib.md ({len(out)} lines)")
