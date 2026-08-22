"""Package a p2w program as a Mojo file: prelude, then the program's
top-level `def`s verbatim, then everything else wrapped in `def main()`.

Mojo executes `main()`, not top-level statements, so this is the one
packaging transform the bridge needs. Hoisting the defs is safe for the
programs the profile passes: p2w top-level defs are unconditional, and the
wrapped statements run after all of them exist — same define-before-use
order the original had.
"""

import sys


def wrap(prelude_path: str, src_path: str) -> str:
    with open(prelude_path, encoding="utf-8") as f:
        prelude = f.read().rstrip()
    with open(src_path, encoding="utf-8") as f:
        lines = f.read().splitlines()

    defs: list[str] = []
    body: list[str] = []
    i = 0
    while i < len(lines):
        if lines[i].startswith("def "):
            block = [lines[i]]
            i += 1
            while i < len(lines) and (
                lines[i].startswith((" ", "\t")) or lines[i].strip() == ""
            ):
                block.append(lines[i])
                i += 1
            while block and block[-1].strip() == "":
                block.pop()
            defs.append("\n".join(block))
        else:
            body.append(lines[i])
            i += 1

    main = ["def main():"]
    wrote = False
    for ln in body:
        if ln.strip():
            main.append("    " + ln)
            wrote = True
        else:
            main.append("")
    if not wrote:
        main.append("    pass")

    parts = [prelude] + defs + ["\n".join(main)]
    return "\n\n\n".join(parts) + "\n"


if __name__ == "__main__":
    sys.stdout.write(wrap(sys.argv[1], sys.argv[2]))
