#!/usr/bin/env python3
"""Seeded random-program generator for differential fuzzing (tools/fuzz_native.sh).

Generates programs in the rust-p2w subset that are SAFE BY CONSTRUCTION —
deterministic, terminating, trap-free, and semantics-identical between CPython
and the native backend *if the compiler is correct* — so any output difference
is a real finding, not generator noise:

- ints are magnitude-tracked (never near the >30-bit boxing gap);
- `//` and `%` take non-negative left operands and literal 1..9 divisors
  (sidesteps truncated-vs-floored divergence and division by zero);
- strings are ASCII-only (our strings are byte-based; len() would diverge on
  multi-byte code points — a known, documented limitation, not a fuzz target);
- floats are excluded (formatting is a separate compatibility axis);
- all indexing is in bounds (list lengths are tracked statically);
- loops are bounded by construction; iterated lists are never mutated in-body.

Generation is deliberately WEIGHTED toward the drop-reuse machinery this crate
ships (comprehension chains, literal reassignment, `x = x + e` append/extend,
aliases that must force copy paths) — the point is to attack the risky paths.

Usage: python3 tools/gen_program.py SEED   (writes the program to stdout)
No third-party dependencies (plain stdlib `random`), reproducible per seed.
"""

import random
import sys

MAX_MAG = 10**7  # keep every int well under 2^29 (the inline-int comfort zone)


class Gen:
    def __init__(self, seed: int):
        self.r = random.Random(seed)
        self.ints = {}  # name -> magnitude bound
        self.strs = []  # names
        self.lists = {}  # name -> (length, element magnitude bound)
        self.lines = []
        self.n = 0

    def name(self, prefix):
        self.n += 1
        return f"{prefix}{self.n}"

    # --- int expressions (magnitude-tracked) --------------------------------

    def int_atom(self, extra_atoms):
        """(text, bound, nonneg) — literal, int var, len(list), or a loop var.
        `nonneg` is a *guarantee*, not a guess: vars are treated as sign-unknown
        (they may hold results of subtraction)."""
        choices = ["lit"]
        if self.ints:
            choices += ["var"] * 2
        if self.lists:
            choices.append("len")
        if extra_atoms:
            choices.append("loop")
        kind = self.r.choice(choices)
        if kind == "var":
            v = self.r.choice(sorted(self.ints))
            return v, self.ints[v], False
        if kind == "len":
            l = self.r.choice(sorted(self.lists))
            return f"len({l})", 60, True
        if kind == "loop":
            return self.r.choice(extra_atoms), 10, True
        k = self.r.randint(0, 99)
        return str(k), k, True

    def int_expr(self, extra_atoms=(), depth=2):
        """(text, bound). Ops that would exceed MAX_MAG are REJECTED (never
        clamp a bound downward — the real value wouldn't shrink with it), and
        `//`/`%` only apply when the left side is provably non-negative
        (Python's floored vs a backend's truncated division diverge below 0 —
        a documented no-go zone, not a fuzz target)."""
        text, bound, nonneg = self.int_atom(extra_atoms)
        while depth > 0 and self.r.random() < 0.6:
            depth -= 1
            op = self.r.choice(["+", "-", "+", "*", "//", "%"])
            if op == "*":
                rt, rb, rn = self.int_atom(extra_atoms)
                if bound * max(rb, 1) > MAX_MAG:
                    continue
                text, bound, nonneg = f"({text} * {rt})", bound * max(rb, 1), nonneg and rn
            elif op in ("//", "%"):
                if not nonneg:
                    continue
                d = self.r.randint(1, 9)
                text = f"({text} {op} {d})"  # bound holds or shrinks; stays nonneg
            else:
                rt, rb, rn = self.int_atom(extra_atoms)
                if bound + rb > MAX_MAG:
                    continue
                nonneg = nonneg and rn and op == "+"
                text, bound = f"({text} {op} {rt})", bound + rb
        return text, bound

    # --- statements ----------------------------------------------------------

    def new_int(self):
        v = self.name("v")
        text, bound = self.int_expr()
        self.ints[v] = bound
        self.lines.append(f"{v} = {text}")

    def reassign_int(self):
        if not self.ints:
            return self.new_int()
        v = self.r.choice(sorted(self.ints))
        text, bound = self.int_expr()
        self.ints[v] = bound
        self.lines.append(f"{v} = {text}")

    def new_str(self):
        s = self.name("s")
        self.strs.append(s)
        lit = "".join(self.r.choice("abcxyz") for _ in range(self.r.randint(1, 4)))
        self.lines.append(f'{s} = "{lit}"')

    def append_str(self):
        # The p2w_add_assign path: s = s + "lit" (occasionally s = s + s).
        if not self.strs:
            return self.new_str()
        s = self.r.choice(self.strs)
        if self.r.random() < 0.15:
            self.lines.append(f"{s} = {s} + {s}")
        else:
            lit = "".join(self.r.choice("mnop") for _ in range(self.r.randint(1, 3)))
            self.lines.append(f'{s} = {s} + "{lit}"')

    def alias_str(self):
        if not self.strs:
            return self.new_str()
        a = self.name("s")
        src = self.r.choice(self.strs)
        self.strs.append(a)
        self.lines.append(f"{a} = {src}")

    def new_list(self):
        l = self.name("l")
        k = self.r.randint(1, 4)
        items = [str(self.r.randint(0, 99)) for _ in range(k)]
        self.lists[l] = (k, 99)
        self.lines.append(f"{l} = [{', '.join(items)}]")

    def reassign_list_literal(self):
        # The try_reuse_literal path: same or different length on purpose.
        if not self.lists:
            return self.new_list()
        l = self.r.choice(sorted(self.lists))
        k = self.r.randint(1, 4)
        items = [str(self.r.randint(0, 99)) for _ in range(k)]
        self.lists[l] = (k, 99)
        self.lines.append(f"{l} = [{', '.join(items)}]")

    def append_list(self, extra_atoms=(), forbid=None, bump=True):
        """`bump=False` inside conditional bodies: the tracked length is a
        guaranteed MINIMUM (used for in-bounds indexing), and a branch may not
        execute — growth there must not raise the minimum."""
        pool = [l for l in sorted(self.lists) if l != forbid]
        if not pool:
            return None
        l = self.r.choice(pool)
        text, bound = self.int_expr(extra_atoms, depth=1)
        if bound > 10**4:
            # Keep element magnitudes small so for-each accumulators over a
            # few dozen elements stay far from the inline-int ceiling.
            text, bound = str(self.r.randint(0, 99)), 99
        n, mag = self.lists[l]
        self.lists[l] = (n + (1 if bump else 0), max(mag, bound))
        self.lines.append(f"{l}.append({text})")
        return l

    def extend_list(self):
        # The p2w_add_assign list path: l = l + [lits].
        if not self.lists:
            return self.new_list()
        l = self.r.choice(sorted(self.lists))
        k = self.r.randint(1, 2)
        items = [str(self.r.randint(0, 99)) for _ in range(k)]
        n, mag = self.lists[l]
        self.lists[l] = (n + k, mag)
        self.lines.append(f"{l} = {l} + [{', '.join(items)}]")

    def alias_list(self):
        if not self.lists:
            return self.new_list()
        a = self.name("l")
        src = self.r.choice(sorted(self.lists))
        self.lists[a] = self.lists[src]
        self.lines.append(f"{a} = {src}")

    def comprehension(self):
        # The try_reuse_map path: additive elements only (magnitude-safe).
        if not self.lists:
            return self.new_list()
        src = self.r.choice(sorted(self.lists))
        dst = self.name("l")
        k = self.r.randint(1, 99)
        op = self.r.choice(["+", "-"])
        n, mag = self.lists[src]
        self.lists[dst] = (n, mag + k)
        self.lines.append(f"{dst} = [x {op} {k} for x in {src}]")

    def slice_str(self):
        # The p2w_slice_assign path: s = s[1:] (peel) and friends. Slicing is
        # total (any bounds on any length), so no length tracking is needed
        # for strings. Occasionally a NEW destination (the dying-source form).
        if not self.strs:
            return self.new_str()
        s = self.r.choice(self.strs)
        form = self.r.choice(["[1:]", "[:3]", "[::2]", "[1:3]", "[::-1]"])
        if self.r.random() < 0.25:
            dst = self.name("s")
            self.strs.append(dst)
            self.lines.append(f"{dst} = {s}{form}")
        else:
            self.lines.append(f"{s} = {s}{form}")

    def slice_list(self):
        # List slice-consume. The tracked MINIMUM length shrinks monotonically
        # under each form (min is monotone), so in-bounds indexing stays sound.
        if not self.lists:
            return self.new_list()
        l = self.r.choice(sorted(self.lists))
        n, mag = self.lists[l]
        form, keep = self.r.choice(
            [("[1:]", max(0, n - 1)), ("[:2]", min(n, 2)), ("[::2]", (n + 1) // 2)]
        )
        if self.r.random() < 0.25:
            dst = self.name("l")
            self.lists[dst] = (keep, mag)
            self.lines.append(f"{dst} = {l}{form}")
        else:
            self.lists[l] = (keep, mag)
            self.lines.append(f"{l} = {l}{form}")

    def print_something(self):
        pools = []
        if self.ints:
            pools.append("int")
        if self.strs:
            pools.append("str")
        if self.lists:
            pools += ["list", "idx"]
        if not pools:
            return self.new_int()
        kind = self.r.choice(pools)
        if kind == "int":
            self.lines.append(f"print({self.r.choice(sorted(self.ints))})")
        elif kind == "str":
            s = self.r.choice(self.strs)
            self.lines.append(f"print({s})" if self.r.random() < 0.7 else f"print(len({s}))")
        elif kind == "list":
            l = self.r.choice(sorted(self.lists))
            self.lines.append(f"print({l})" if self.r.random() < 0.6 else f"print(len({l}))")
        else:
            l = self.r.choice(sorted(self.lists))
            n, _ = self.lists[l]
            self.lines.append(f"print({l}[{self.r.randint(0, n - 1)}])")

    def simple_block_stmt(self, extra_atoms, forbid=None):
        """Statements safe inside if/for bodies: list growth never raises the
        tracked minimum length, and int reassigns are the bounded
        `v = v + k` / `v = k` forms (a loop body runs at most 5 times, so the
        tracked bound grows by at most 5*99 — no compounding blowups)."""
        pick = self.r.random()
        if pick < 0.35:
            self.print_something()
        elif pick < 0.6 and self.strs:
            self.append_str()
        elif pick < 0.85:
            if self.append_list(extra_atoms, forbid=forbid, bump=False) is None:
                # No list to append to: never leave a block body empty.
                self.print_something()
        else:
            if not self.ints:
                return self.print_something()
            v = self.r.choice(sorted(self.ints))
            k = self.r.randint(0, 99)
            if self.r.random() < 0.5 and self.ints[v] + 5 * 99 <= MAX_MAG:
                self.ints[v] = self.ints[v] + 5 * 99
                self.lines.append(f"{v} = {v} + {k}")
            else:
                self.ints[v] = max(self.ints[v], k)  # branch may not run: join
                self.lines.append(f"{v} = {k}")

    def if_block(self):
        if not self.ints:
            return self.new_int()
        v = self.r.choice(sorted(self.ints))
        cmpop = self.r.choice(["<", ">", "<=", ">=", "==", "!="])
        k = self.r.randint(0, 99)
        self.lines.append(f"if {v} {cmpop} {k}:")
        body_at = len(self.lines)
        for _ in range(self.r.randint(1, 2)):
            self.simple_block_stmt(())
        for i in range(body_at, len(self.lines)):
            self.lines[i] = "    " + self.lines[i]
        if self.r.random() < 0.5:
            self.lines.append("else:")
            body_at = len(self.lines)
            self.simple_block_stmt(())
            for i in range(body_at, len(self.lines)):
                self.lines[i] = "    " + self.lines[i]

    def for_range(self):
        i = self.name("i")
        stop = self.r.randint(1, 5)
        self.lines.append(f"for {i} in range({stop}):")
        body_at = len(self.lines)
        for _ in range(self.r.randint(1, 2)):
            self.simple_block_stmt((i,))
        for k in range(body_at, len(self.lines)):
            self.lines[k] = "    " + self.lines[k]

    def for_each(self):
        if not self.lists:
            return self.new_list()
        src = self.r.choice(sorted(self.lists))
        e = self.name("e")
        acc = self.name("v")
        self.ints[acc] = 10**6  # sums of small tracked elements over short lists
        self.lines.append(f"{acc} = 0")
        self.lines.append(f"for {e} in {src}:")
        # Never mutate the iterated list in the body.
        self.lines.append(f"    {acc} = {acc} + {e}")
        if self.r.random() < 0.5:
            self.lines.append(f"print({acc})")

    def generate(self):
        stmts = [
            (self.new_int, 2),
            (self.new_str, 2),
            (self.new_list, 3),
            (self.reassign_int, 2),
            (self.append_str, 4),  # weighted: the reuse paths
            (self.alias_str, 1),
            (self.reassign_list_literal, 3),
            (self.append_list, 2),
            (self.extend_list, 3),
            (self.alias_list, 2),
            (self.comprehension, 4),
            (self.slice_str, 3),
            (self.slice_list, 3),
            (self.print_something, 4),
            (self.if_block, 2),
            (self.for_range, 2),
            (self.for_each, 2),
        ]
        weighted = [f for f, w in stmts for _ in range(w)]
        for _ in range(self.r.randint(10, 22)):
            self.r.choice(weighted)()
        # Print every live variable so all end state is observable.
        for v in sorted(self.ints):
            self.lines.append(f"print({v})")
        for s in self.strs:
            self.lines.append(f"print({s})")
        for l in sorted(self.lists):
            self.lines.append(f"print({l})")
        return "\n".join(self.lines) + "\n"


if __name__ == "__main__":
    seed = int(sys.argv[1]) if len(sys.argv) > 1 else 1
    sys.stdout.write(Gen(seed).generate())
