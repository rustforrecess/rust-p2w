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
        self.classes = {}  # class name -> __init__ arity (excl. self)
        self.objs = {}  # instance name -> class name
        self.dicts = {}  # name -> list of known int keys (in-bounds reads)
        self.sets = {}  # name -> "int" | "str" (element kind)
        self.tuples = {}  # name -> length
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

    def _scalar(self):
        """A small, always-safe literal element/value: a number 0-99 or a short
        ASCII string. Only ever printed or summed, never fed into tracked
        arithmetic, so no magnitude concern; scalar so no cycle is possible."""
        if self.r.random() < 0.6:
            return str(self.r.randint(0, 99))
        return '"' + "".join(self.r.choice("abcde") for _ in range(self.r.randint(1, 3))) + '"'

    def new_dict(self):
        # int keys (0-9, so updates collide); scalar values. Display order is
        # not diffed — only len / in-bounds reads are printed (deterministic).
        d = self.name("d")
        keys = self.r.sample(range(10), self.r.randint(1, 3))
        items = ", ".join(f"{k}: {self._scalar()}" for k in keys)
        self.dicts[d] = keys
        self.lines.append(f"{d} = {{{items}}}")

    def dict_op(self):
        if not self.dicts:
            return self.new_dict()
        d = self.r.choice(sorted(self.dicts))
        keys = self.dicts[d]
        pick = self.r.random()
        if pick < 0.4 and keys:
            # Update an EXISTING key -> exercises dict_set's release-of-old.
            k = self.r.choice(keys)
            self.lines.append(f"{d}[{k}] = {self._scalar()}")
        elif pick < 0.65:
            # Insert a possibly-new key.
            k = self.r.randint(0, 9)
            if k not in keys:
                keys.append(k)
            self.lines.append(f"{d}[{k}] = {self._scalar()}")
        elif pick < 0.85 and keys:
            self.lines.append(f"print({d}[{self.r.choice(keys)}])")  # in-bounds read
        else:
            self.lines.append(f"print(len({d}))")

    def new_set(self):
        # Small element range so duplicates dedup (exercises the dedup-release).
        # Display order differs from CPython, so a set is NEVER printed whole —
        # only order-independent observations (len / membership / sum / op-size).
        s = self.name("st")
        if self.r.random() < 0.6:
            self.sets[s] = "int"
            elems = ", ".join(str(self.r.randint(0, 4)) for _ in range(self.r.randint(2, 5)))
        else:
            self.sets[s] = "str"
            elems = ", ".join(
                '"' + self.r.choice("abcd") + '"' for _ in range(self.r.randint(2, 5))
            )
        self.lines.append(f"{s} = {{{elems}}}")

    def set_op(self):
        if not self.sets:
            return self.new_set()
        s = self.r.choice(sorted(self.sets))
        kind = self.sets[s]
        peers = [t for t in sorted(self.sets) if self.sets[t] == kind and t != s]
        pick = self.r.random()
        if pick < 0.3:
            self.lines.append(f"print(len({s}))")
        elif pick < 0.55:
            probe = str(self.r.randint(0, 5)) if kind == "int" else '"' + self.r.choice("abcde") + '"'
            self.lines.append(f"print({probe} in {s})")
        elif pick < 0.75 and kind == "int":
            # Sum is commutative -> order-independent -> matches CPython.
            acc = self.name("v")
            e = self.name("e")
            self.lines.append(f"{acc} = 0")
            self.lines.append(f"for {e} in {s}:")
            self.lines.append(f"    {acc} = {acc} + {e}")
            self.lines.append(f"print({acc})")
        elif peers:
            t = self.r.choice(peers)
            op = self.r.choice(["&", "|", "-", "^"])
            self.lines.append(f"print(len({s} {op} {t}))")  # size is order-free
        else:
            self.lines.append(f"print(len({s}))")

    def new_tuple(self):
        # Tuples are ordered -> printing IS deterministic and diff-safe.
        t = self.name("t")
        n = self.r.randint(1, 4)
        elems = ", ".join(self._scalar() for _ in range(n))
        if n == 1:
            elems += ","  # 1-tuple
        self.tuples[t] = n
        self.lines.append(f"{t} = ({elems})")

    def tuple_op(self):
        if not self.tuples:
            return self.new_tuple()
        t = self.r.choice(sorted(self.tuples))
        n = self.tuples[t]
        pick = self.r.random()
        if pick < 0.35:
            self.lines.append(f"print({t}[{self.r.randint(0, n - 1)}])")  # in-bounds
        elif pick < 0.6:
            self.lines.append(f"print(len({t}))")
        elif pick < 0.8:
            self.lines.append(f"print({t})")  # ordered display, diff-safe
        else:
            probe = str(self.r.randint(0, 99))
            self.lines.append(f"print({probe} in {t})")

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

    def churn_type(self):
        # The slot-inference demote adversary: rebind an int var to a string
        # (or back), moving it between pools so later statements use it at
        # its NEW type. Top-level only — conditional churn would make the
        # static type unknowable — so this is registered in generate() and
        # must never go into simple_block_stmt.
        if self.ints and self.r.random() < 0.6:
            v = self.r.choice(sorted(self.ints))
            del self.ints[v]
            lit = "".join(self.r.choice("abc") for _ in range(2))
            self.strs.append(v)
            self.lines.append(f'{v} = "{lit}"')
        elif self.strs:
            v = self.r.choice(self.strs)
            self.strs.remove(v)
            k = self.r.randint(0, 99)
            self.ints[v] = k
            self.lines.append(f"{v} = {k}")
        else:
            return self.new_int()

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
            # In-bounds index print needs a list whose tracked MINIMUM length
            # is >= 1 (slicing can shrink the minimum to 0).
            pool = [l for l in sorted(self.lists) if self.lists[l][0] >= 1]
            if not pool:
                l = self.r.choice(sorted(self.lists))
                self.lines.append(f"print(len({l}))")
                return
            l = self.r.choice(pool)
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

    def setup_classes(self):
        """Emit 1-2 class definitions at the TOP of the program (Python needs
        them before use) and record each instance's constructor arity. Fixed
        SAFE templates: attributes are only numbers/strings (categorically no
        reference cycles → no leak), argument values are 0-99 (a `total()` of a
        few attrs stays < 300, no i32 wrap), and every dunder used is one the
        backend dispatches. Also exercises inheritance + super()."""
        base = self.name("C")
        # Base: __init__(self, a, b), total(), __str__, __eq__ (all safe).
        self.lines += [
            f"class {base}:",
            "    def __init__(self, a, b):",
            "        self.a = a",
            "        self.b = b",
            "    def total(self):",
            "        return self.a + self.b",
            "    def __str__(self):",
            '        return "C(" + str(self.a) + "," + str(self.b) + ")"',
            "    def __eq__(self, o):",
            "        return self.a == o.a",
        ]
        self.classes[base] = 2  # __init__ arity (excl. self)
        if self.r.random() < 0.6:
            sub = self.name("D")
            self.lines += [
                f"class {sub}({base}):",
                "    def __init__(self, a, b, c):",
                "        super().__init__(a, b)",
                "        self.c = c",
                "    def total(self):",  # override, exercises dispatch
                "        return self.a + self.b + self.c",
            ]
            self.classes[sub] = 3  # inherits __str__/__eq__/a/b, adds c

    def new_object(self):
        if not self.classes:
            return self.new_int()
        cn = self.r.choice(sorted(self.classes))
        args = ", ".join(str(self.r.randint(0, 99)) for _ in range(self.classes[cn]))
        o = self.name("o")
        self.objs[o] = cn
        self.lines.append(f"{o} = {cn}({args})")

    def object_op(self):
        if not self.objs:
            return self.new_object()
        o = self.r.choice(sorted(self.objs))
        pick = self.r.random()
        if pick < 0.3:
            self.lines.append(f"print({o}.total())")  # method dispatch
        elif pick < 0.5:
            self.lines.append(f"print({o}.a)")  # attr read (a always exists)
        elif pick < 0.7:
            # Attr set to a small number, then observed (dynamic dict attrs).
            self.lines.append(f"{o}.a = {self.r.randint(0, 99)}")
            self.lines.append(f"print({o}.a)")
        else:
            # __eq__ against another object of any class (both have `a`).
            other = self.r.choice(sorted(self.objs))
            self.lines.append(f"print({o} == {other})")

    def generate(self):
        # Classes (if any) must be defined before the statement loop uses them.
        if self.r.random() < 0.5:
            self.setup_classes()
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
            (self.churn_type, 2),
            (self.new_object, 3),
            (self.object_op, 4),
            (self.new_dict, 2),
            (self.dict_op, 4),
            (self.new_set, 2),
            (self.set_op, 4),
            (self.new_tuple, 2),
            (self.tuple_op, 4),
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
        # Print every instance (default/inherited __str__) so the object graph
        # is fully observed and its RC cascade must end at live == 0.
        for o in sorted(self.objs):
            self.lines.append(f"print({o})")
        # Tuples are ordered, so their display matches CPython; dicts/sets are
        # observed only via len/reads/membership above (display order differs).
        for t in sorted(self.tuples):
            self.lines.append(f"print({t})")
        return "\n".join(self.lines) + "\n"


if __name__ == "__main__":
    seed = int(sys.argv[1]) if len(sys.argv) > 1 else 1
    sys.stdout.write(Gen(seed).generate())
