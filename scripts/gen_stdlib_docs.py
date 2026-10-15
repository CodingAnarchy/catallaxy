#!/usr/bin/env python3
"""Generates docs/stdlib.md from the prelude sources (the single source of truth for the API).

Each declaration is listed with its documentation comment: the block of `#` lines right above it
(no blank line between), as the language server shows it in hover and completion.
"""
import argparse
import pathlib
import re
import sys

root = pathlib.Path(__file__).resolve().parent.parent
prelude = root / "crates/catallaxy-compiler/src/prelude"
target = root / "docs/stdlib.md"


def is_internal(line):
    # Functions starting with `_` are implementation details (e.g. `__fiber_spawn`, `Deque._grow`).
    return re.match(r"(?:pub )?(?:native )?fn _", line.strip()) is not None


def is_directive(comment):
    body = comment.lstrip("#").strip()
    return comment.startswith("#!") or body.startswith(("allow(", "expect:", "exit:"))


def sig_of(line):
    line = line.strip()
    line = re.sub(r"\s*\{.*$", "", line) if "{" in line and not line.startswith("{") else line
    return line.replace("native fn ", "fn ").replace("pub fn ", "fn ").replace("pub native fn ", "fn ")


def markdown(lines):
    """As the compiler's `parser::doc_markdown`: lines indented by two or more spaces are example
    code, fenced as `catallaxy` without that indent; the rest is Markdown as written."""
    out = []
    i = 0
    while i < len(lines):
        if not lines[i].startswith("  "):
            out.append(lines[i])
            i += 1
            continue
        end = j = i
        while j < len(lines) and (lines[j].startswith("  ") or not lines[j].strip()):
            if lines[j].startswith("  "):
                end = j + 1
            j += 1
        out += ["```catallaxy"] + [l[2:].rstrip() for l in lines[i:end]] + ["```"]
        i = end
    return out


def doc_text(lines, indent=""):
    """The comment lines (without `#`) as Markdown, each line prefixed by `indent`."""
    while lines and not lines[0].strip():
        lines = lines[1:]
    while lines and not lines[-1].strip():
        lines = lines[:-1]
    return [f"{indent}{l}".rstrip() for l in markdown(lines)]


def generate():
    out = ["# Catallaxy Standard Library", "",
           "Generated from the prelude by `scripts/gen_stdlib_docs.py`; do not edit by hand.",
           "Everything here is available in every module without an import. The text under an",
           "entry is its documentation comment, which editors also show on hover and in completion.", ""]

    def item(text, doc):
        out.append(f"- {text}")
        body = doc_text(doc, "  ")
        if body:
            out.append("")
            out.extend(body)
            out.append("")

    for f in ["core.cx", "builtins.cx"]:
        lines = (prelude / f).read_text().splitlines()
        depth = 0
        section = None
        doc = []
        for raw in lines:
            line = raw.strip()
            if depth <= 1 and line.startswith("#"):
                if not is_directive(line):
                    t = line[1:]
                    doc.append(t[1:] if t.startswith(" ") else t)
                depth += raw.count("{") - raw.count("}")
                continue
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
                        if bounded:
                            head = f"{name} (where {', '.join(bounded)})"
                    out += ["", f"## {head}", ""]
                    body = doc_text(doc)
                    if body:
                        out += body + [""]
                    section = head
                elif line.startswith(("pub native fn", "pub fn", "native fn", "fn")) and not is_internal(line):
                    if section != "Global functions":
                        out += ["", "## Global functions", ""]
                        section = "Global functions"
                    item(f"`{sig_of(line)}`", doc)
            elif depth == 1 and section:
                if line.startswith(("fn ", "native fn ", "pub fn ", "mut fn")):
                    if not is_internal(line):
                        item(f"`{sig_of(line)}`", doc)
                elif re.match(r"^[a-z]\w*: ", line):
                    # Fields starting with `_` are implementation details (e.g. `Fiber._result`).
                    item(f"field `{line}`", doc)
                elif line.startswith("const "):
                    item(f"`{line}`", doc)
                elif re.match(r"^[A-Z]\w*(\(.*\))?,?$", line):
                    item(f"variant `{line.rstrip(',')}`", doc)
            doc = []
            depth += raw.count("{") - raw.count("}")
            # Strings containing braces are rare in the prelude; interpolation braces balance.

    text = "\n".join(out)
    while "\n\n\n" in text:
        text = text.replace("\n\n\n", "\n\n")
    return text.rstrip("\n") + "\n"


def main():
    ap = argparse.ArgumentParser(
        description="Generate docs/stdlib.md from the prelude sources and their documentation comments.")
    ap.add_argument("--check", action="store_true",
                    help="do not write; exit with status 1 if docs/stdlib.md is out of date")
    args = ap.parse_args()
    text = generate()
    if args.check:
        if target.read_text() != text:
            print("docs/stdlib.md is out of date: run scripts/gen_stdlib_docs.py", file=sys.stderr)
            return 1
        print("docs/stdlib.md is up to date")
        return 0
    target.write_text(text)
    print(f"wrote docs/stdlib.md ({text.count(chr(10))} lines)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
