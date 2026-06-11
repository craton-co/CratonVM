# CratonVM — Documentation Review (Fable, 2026-06-10)

Scope: every root markdown/governance file, the `docs/` tree (external set,
`docs/gpu/`, `docs/gaps/`, `docs/internal/`). Static review only. Produced inline
by the orchestrator after the `docs` sub-agent was lost to the session limit.

## Summary

The **maintained, user-facing documentation set is genuinely strong**: README,
ARCHITECTURE, CONTRIBUTING, SECURITY, RELEASING, GOVERNANCE, CODE_OF_CONDUCT and
the `docs/` external set (INSTALL, CONFIG, PLATFORMS, TROUBLESHOOTING,
JDK_COVERAGE, CRYPTO_STATUS, gpu/) are well-written, honest about limitations
(the README "Status & Known Limitations" section is refreshingly candid: "NOT
certified", "MUST NOT be used to run untrusted Java code", crypto not
production-ready, a tracked JIT crash), and cross-link sensibly. Both `docs/README.md`
and `docs/internal/README.md` correctly mark the internal notes as **non-normative**.

The problems are **(1) a large set of broken relative links** in the two index
documents (README.md and docs/README.md point at files that were moved under
`docs/internal/`, plus two files that do not exist at all — `BUILD_GUIDE.md` and
`TRADEMARKS.md`); **(2) a direct factual contradiction** between RELEASING.md and
the actual `Cargo.toml` publish configuration; **(3) CI claims that are false**
(CONTRIBUTING/RELEASING describe an active `.github/workflows/ci.yml` that does
not exist — the workflows are parked under `.github/.wf/`); and **(4) a very large
`docs/internal/` tree (~150 files)** of session handoffs, `continue_prompt_*`
notes, and round-by-round audit logs that — while honestly flagged as scratch —
carry machine paths and clutter, and should be triaged before a public release.

---

## Broken / dangling links (HIGH — these 404 in a public repo)

Verified by file-existence check:

| Referenced as | Referenced from | Reality |
|---|---|---|
| `BUILD_GUIDE.md` | README.md:230, CONTRIBUTING.md:264, docs/INSTALL.md:709 | **Does not exist** anywhere |
| `TRADEMARKS.md` | README.md:249 | **Does not exist** (also flagged by oss-readiness) |
| `docs/javafx-status.md` | README.md:163 | Lives at `docs/internal/javafx-status.md` |
| `docs/embedding.md` | README.md:233 | Lives at `docs/internal/embedding.md` |
| `docs/gc-tuning.md` | README.md:234 | Lives at `docs/internal/gc-tuning.md` |
| `docs/jvm-no-synthetic-stubs.md` | README.md:241 | Lives at `docs/internal/app-jvm-bugs/jvm-no-synthetic-stubs.md` |
| `lock-order.md` | docs/README.md:590 | **No `docs/lock-order.md`** (referenced as a design note) |
| `jck-compliance.md` | docs/README.md:578 | Lives at `docs/internal/jck-compliance.md` |
| `feature_roadmap_*.md` | docs/README.md:592-593 | Live under `docs/internal/` |
| `bc-ec-mod-mododdinverse-investigation.md`, `tomcat-selector-investigation.md`, `jit-safepoint-revert.md` | docs/README.md "Open investigations" | Live under `docs/internal/` (and `…/app-jvm-bugs/`) |

Root cause: a docs reorg moved files into `docs/internal/` but the two index
files were not updated. **Fix:** either move the genuinely user-facing targets
(embedding, gc-tuning, javafx-status) back up to `docs/`, or repoint the links;
create `BUILD_GUIDE.md` and `TRADEMARKS.md` or remove the references. A markdown
link-checker in CI would prevent recurrence.

## Factual contradictions (HIGH)

1. **RELEASING.md §3 vs Cargo.toml.** RELEASING.md:504 states *"The workspace
   currently sets `publish = false` at `[workspace.package]`, so nothing is pushed
   to crates.io."* The root `Cargo.toml:6-9` states the **opposite**: *"members do
   NOT inherit a publish gate from here … so every library crate is publishable to
   crates.io."* This is not cosmetic — it gives a false sense of safety against an
   accidental `cargo publish`. Reconcile by actually adding a publish gate (see
   oss-readiness) and making RELEASING.md describe reality.

2. **CI is described as active but is dormant.** CONTRIBUTING.md:278 — *"CI
   (`.github/workflows/ci.yml`) enforces each one … CI runs these on
   ubuntu-latest and windows-latest for every push and pull request."*
   RELEASING.md:479 — *"Wait for CI green … `.github/workflows/ci.yml` runs …"*
   README.md:3 carries a CI **badge** pointing at the same path. There is **no
   `.github/workflows/` directory**; all workflow YAML lives in `.github/.wf/`,
   which GitHub Actions never scans. So the badge is broken and the "CI enforces
   this" claims are false. (Cross-referenced in oss-readiness and scripts reviews.)

## Inconsistencies (MEDIUM)

- **Crate-count / description drift.** README.md:199 says "17 member crates plus a
  `fuzz` harness"; the workspace has 18 members incl. fuzz, matching. But README
  and CONTRIBUTING both describe `craton-gpu` as *"GPU offload runtime
  integration"*, while its Cargo.toml describes it as *"Java annotation sources …
  compiled at build time"* (a build-time annotations crate, 15 LOC). Align the
  one-liner.
- **GC backend claims.** README.md:216 and CONTRIBUTING.md:314 list the gc crate
  as "semi-space, G1, ZGC". Per the gc review and Cargo.toml, ZGC is a
  **`zgc`-feature-gated 1884-LOC stub with no in-tree consumer** and there is no
  G1. The public docs imply three production collectors; reality is a generational
  young/old collector (+ Cheney moving + non-moving sweep). Soften to match.
- **Stat claims are stale/loose.** "~7,200 lines / ~140 bytecodes / 26 rounds" for
  the JIT (the `jit` crate is ~54k LOC; 7.2k is just the x64 core), "~323,000+
  lines of Rust" (workspace src is larger), "3,100+ native registrations" (other
  docs say 3,100; native-builtins alone is ~313k LOC). Not blockers but a public
  README should be internally consistent and dated.
- **scripts/README.md is partly stale**: it documents `build-h2.bat`,
  `build-rwd.bat`, `build-wt.bat` which are not present in `scripts/` (see
  scripts-review.md).
- **"0 clippy warnings"** (README, CONTRIBUTING) is achieved partly by the
  workspace `[lints]` setting `dead_code`/`unused_*`/several rustdoc lints to
  `allow`. True but worth a footnote so it is not read as "lint-clean with default
  lints".

## Audience classification & disposition

**External / ship as-is (after link fixes):** README, ARCHITECTURE, CONTRIBUTING,
CODE_OF_CONDUCT, SECURITY, GOVERNANCE, MAINTAINERS, RELEASING, ROADMAP, SUPPORT,
CHANGELOG, LICENSE, NOTICE, AUTHORS; `docs/{INSTALL,CONFIG,PLATFORMS,TROUBLESHOOTING,JDK_COVERAGE,CRYPTO_STATUS,JIT_OPTIMIZATION,PROFILING,PRESENTATION,README}.md`;
`docs/gpu/*` (coherent, well-structured); `docs/legal.md` (JCK licensing — keep,
relevant).

**Internal (keep but clearly fenced, or move to a private repo/wiki):**
`docs/internal/**` (~150 files). It is *honestly* marked non-normative, which is
the right mitigation if it ships, but it contains: (a) `comparison-handoff/continue_prompt_*`
(18 session-handoff prompts — pure working scratch, recommend **remove**); (b)
round4–round9 per-subsystem audit logs (historical; fine to keep but low public
value); (c) `app-jvm-bugs/*` and blocker maps with machine paths. At minimum scrub
personal paths (oss-readiness found `C:/Users/Victor` in two `docs/gaps/*` and a
script). Decision needed: ship `docs/internal/` as "developer history" or extract
it.

**Borderline:** `docs/gaps/**` — known-bug notes. Could become a curated public
"Known Issues" page; today they read as internal investigations. The stale
`docs/gaps/gap-bintrees18-gc-throughput.md.pre-rewrite.bak` is currently
**untracked** (good) — delete it so it cannot be added.

## Missing for a public OSS repo (ADD)

- `BUILD_GUIDE.md` (referenced 3×) — or fold its content into CONTRIBUTING and
  drop the links.
- `TRADEMARKS.md` — Oracle "Java" is a trademark; a public JVM-compat project
  should carry the standard "Java is a trademark of Oracle" notice (also a
  crates.io/legal nicety). oss-readiness lists this as a warning.
- A **supported-JDK-version statement** that is unambiguous: the README support
  table claims Java 8–25 incl. Java 25 features; pair it with the honest "targets
  broad coverage, not certified" caveat in one place.
- **Benchmark methodology** note (hardware, JDK build, flags, date) co-located with
  the README benchmark table (currently one date line; PRESENTATION.md has more —
  link it from the table).
- A short **THREAT MODEL** paragraph (the vm-core review notes `System.load`
  loads arbitrary native code exactly as a real JVM; say so explicitly given the
  "don't run untrusted code" warning).

## REMOVE / move before open-sourcing

- `docs/gaps/*.pre-rewrite.bak` (stale backup).
- `docs/internal/comparison-handoff/continue_prompt_*.md` (session scratch) — or
  relocate to a private dev repo.
- Scrub `C:/Users/Victor` / `C:/craton/...` machine paths from any doc that ships
  (oss-readiness found them in `docs/gaps/*`).

## Consistency-checker recommendation

Add a CI doc-lint job (once CI is actually wired): a relative-link checker
(`lychee`/`markdown-link-check`) plus a tiny script asserting the crate table in
README/CONTRIBUTING matches `cargo metadata`. Both contradictions above would have
been caught mechanically.
