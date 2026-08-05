# Licensing proposal — avoid commercial capture while keeping reach

> **STATUS: PROPOSAL for review. Nothing here is applied.** All repos are still
> MIT. This is a decision doc, not a change. — Jul 18 2026

## Where we are today
| Repo | License now | Capture protection now |
|---|---|---|
| `rust-p2w` (compiler + analysis **component**) | **MIT** | none (fine — it's meant to be reused) |
| `acornstem-ide` (assembled **product**) | **MIT** | **none** — a company can take the whole IDE and commercialize it today |
| AI a11y tester (future) | — | separate repo, TBD |

Because everything you wrote is **clean-room and solely yours**, you can relicense
your own code freely (MIT → anything). The MIT versions already released stay MIT
for those snapshots, but you control all future releases. (Keep p2w's MIT notice in
`NOTICE` regardless — `rust-p2w` derives from it.)

## The reframe (why *not* AGPL, even though it "protects")
AGPL locks down the **code** — but for this project **the code was never the moat.**
The defensibility is the curriculum, standards alignment (CASE/PXC), assessment
content, teacher trust, brand, community, hosted service, and accumulated evidence.
AGPL protects the wrong asset *and* costs reach (many schools + companies have blanket
AGPL bans, even internal). So it over-protects the commodity while leaving the real
moat exposed either way. **Protect the things a fork can't copy; keep the code open.**

## Recommendation
**Permissive code (Apache-2.0) + AcornSTEM trademark + ecosystem-moat**, with the
reusable component kept as a standalone library. Add a **BSL backstop** on the
product *only if* you decide you want explicit legal teeth against a competing SaaS.

- **`rust-p2w` → Apache-2.0.** (Upgrade from MIT: Apache adds an explicit **patent
  grant** and a **trademark clause** — strictly better for a reusable component;
  still fully permissive / OSI-open / company-friendly.)
- **`acornstem-ide` → Apache-2.0 + trademark** (default), **or BSL** (if you want the
  anti-SaaS clause). Either way: reaches schools AGPL would scare off.
- **AI a11y tester → separate repo, Apache-2.0.**
- **The moat lives in:** the AcornSTEM **trademark** + curriculum/standards/assessment
  content + community + hosted infrastructure + data — none of which a code-fork gets.

## Option matrix
| Option | Reach | Capture protection | Openness | Effort |
|---|---|---|---|---|
| **Apache + trademark + ecosystem (RECOMMENDED)** | max | brand + ecosystem | OSI-open | low |
| Apache + trademark + **nonprofit steward** | max | + institutional + grant funding | OSI-open | med (form entity) |
| **BSL** on product (Apache components) | high (free for schools/non-compete) | explicit anti-SaaS, auto-opens in N yrs | source-available | low–med |
| AGPL product (your first idea) | **reduced** (AGPL bans) | copyleft on code (the non-moat) | OSI-open | low |
| MIT everything (status quo) | max | **none** | OSI-open | none |

## The one architectural rule (non-negotiable if tiering)
**Dependency direction is one-way:** the product may depend *down* on the Apache/MIT
components; the components must **never** depend *up* on any copyleft (AGPL/BSL) code,
or they become tainted for downstream reusers. Today this is clean (`rust-p2w` deps
are permissive: `ryu`). Just never let a component take a copyleft dependency.

## Concrete artifacts (drafts — what would actually change)
1. **`rust-p2w/LICENSE`** → replace MIT text with the standard **Apache-2.0** text
   (apache.org/licenses/LICENSE-2.0.txt), keep `NOTICE`.
2. **`rust-p2w/Cargo.toml`** → `license = "Apache-2.0"`.
3. **`acornstem-ide/LICENSE`** → Apache-2.0 (or the BSL text if that path chosen).
4. **`TRADEMARK.md`** (new, both repos) — the brand-use policy. Draft:
   > The **AcornSTEM** name and logo are trademarks of Jason Smith. The *code* is
   > Apache-2.0 — fork, modify, and build on it freely. The *marks* are not licensed:
   > a fork or derivative may not use the AcornSTEM name/logo, or imply endorsement
   > or official status, without written permission. Rename your fork. Nominative
   > references ("compatible with AcornSTEM") are fine.
5. **`README` license section** — state the split: "Apache-2.0 code, AcornSTEM
   trademarks reserved; see TRADEMARK.md."
6. (If nonprofit path) — assign copyright/marks to the entity; note stewardship.

## Decision points (yours)
1. **Components:** Apache-2.0 (recommended, patent+TM clauses) or keep MIT?
2. **Product:** Apache + trademark (max reach) **or** BSL (legal anti-SaaS, source-available)?
3. **Nonprofit stewardship:** now / later / never? (Fits "not to compete"; unlocks grants; eases handoff.)
4. **Trademark:** register "AcornSTEM" (costs, real protection) or rely on unregistered/common-law to start?

## Rollout if adopted (small, mechanical)
- Swap the two `LICENSE` files + the `Cargo.toml` license field.
- Add `TRADEMARK.md` to both repos + a README license note.
- Keep `NOTICE` (p2w MIT attribution) unchanged.
- One commit per repo; the a11y tester starts fresh in its own repo.

## Not legal advice
This explains how these models generally work. Before filing a trademark, forming a
nonprofit, or committing to BSL vs Apache on the product, run it past an IP/tech
lawyer — the choice and the trademark filing have real consequences.
