# Backlog, 2026-09-17: the bridge-retirement campaign wound down, six lanes start here

**Status: this is an index and groundwork assignment, not a defect record.**
**Owner: this lane, groundwork/integration role — no source prefix, so it
cannot collide with lanes 2-7 below.**

## Why this page exists

`docs/internal/jdk-only/lane-0-integration-and-gates-RETIRED-20260916.md` and
its seven siblings (L1, L2, L3, L5, L6, L7, LT — all `RETIRED` between
2026-09-10 and 2026-09-16) closed the campaign whose goal was *"remove all
synthetic bridges shadowing real bytecode from `--jdk-only` mode"* over its
5,549-row goal population. **`INDEX.md` and `README.md` in this directory
predate all of that** — they were last rebuilt 2026-08-17 and 2026-08-22 and
already warn, correctly, not to trust their own headline counts. Neither
mentions that the campaign finished. A reader who starts from either page
today reconstructs a five-week-stale picture.

What is actually true on 2026-09-17:

* **Lanes L0, L1, L2, L3, L5, L6, L7, LT: RETIRED.** Their ownership
  prefixes (`docs/internal/jdk-only/lane-0-integration-and-gates-RETIRED-20260916.md`
  §2) are now free, and each retirement page names what it left open with a
  blocker (kind-map staleness, `BASELINE_INTRINSICS` off-by-one, a handful of
  named residual defects). This page and its six siblings are where those
  residuals get picked up.
* **Lane L4 (`java/io/`, `java/nio/`, `sun/nio/`, `jdk/internal/foreign`) is
  still ACTIVE, not retired.** Its own page,
  [`jdk-only-lanes/lane-4-io-nio-foreign.md`](lane-4-io-nio-foreign.md),
  ends with one named, unclaimed follow-up (`WindowsPath.type`/`.root` are
  declared but never written). [`lane-4-continuation-windows-path-fields-20260917.md`](lane-4-continuation-windows-path-fields-20260917.md)
  picks that up as its own page rather than editing the 1,641-line original.
* **The six-acceptance-criteria measurement**
  ([`the-six-acceptance-criteria-measured-20260910.md`](the-six-acceptance-criteria-measured-20260910.md))
  is the current authority on where `--jdk-only` stands against
  `docs/feature-designs/jdk-only-mode.md` §11, **not** `README.md` §1. Read it,
  not the older normative-contract summary, before citing a criterion.

## The six lanes this pass opens

| lane | scope | page | matching worktree, if any |
|---|---|---|---|
| classloading & reflection surface | **RETIRED 2026-09-18** — all four findings fixed and verified against their own probes plus the full 3-arm corpus. See the retired `lane-classloading-reflection-surface-20260917-RETIRED-20260918.md` write-up under `docs/internal/jdk-only/`. | — | — |
| JCA / KeyStore / network policy | **RETIRED 2026-09-17**, same day it opened — JCEKS entry-password scheme (KS-7) closed by a third crypto scheme reverse-engineered from JDK source, the link-local outbound guard narrowed to the specific known cloud-metadata addresses. See the retired `lane-jca-keystore-network-policy-RETIRED-20260917.md` write-up. The source defect page stays OPEN for its own unrelated residual (KS-2/KS-3, the `Key.equals`/round-trip identity gap this lane's own opening text mis-scoped as landed — see that retirement's correction note). | — | `jdk-lanes-net-security-*`
| concurrency contracts | **RETIRED 2026-09-17**, same day it opened — `ForkJoinTask` double-execution closed by a VM-internal side-table claim, `ScheduledThreadPoolExecutor`'s keep-arm retired behind a real-JDK Spring corpus A/B. See the retired `lane-concurrency-forkjoin-scheduler-RETIRED-20260917.md` write-up. | — | —
| `MethodHandles` combinators | 16 of 56 combinators wrong, mode-independent | [`lane-methodhandles-combinator-sweep-20260917.md`](lane-methodhandles-combinator-sweep-20260917.md) | —
| `StringBuilder` JIT intrinsic | **RETIRED 2026-09-17** — the one thing blocking 123 free-correctness retirement rows | `lane-stringbuilder-jit-intrinsic-RETIRED-20260917.md` (moved to `docs/internal/jdk-only/`) | —
| lane 4 continuation | `WindowsPath.type`/`.root`, the fourth defect in that lane's own chain | [`lane-4-continuation-windows-path-fields-20260917.md`](lane-4-continuation-windows-path-fields-20260917.md) | `jdk-lanes-windowspath-typeroot-20260917`, `jdk-lanes-io-nio-foreign-*` |

**On the matching-worktree column**: as of 2026-09-17 there are several
`.claude/worktrees/jdk-lanes-*` directories checked out at `dev`'s tip with
zero commits of their own — pre-provisioned, unstarted lanes. Three of them
line up with topics this pass independently arrived at from the open-defect
evidence, which is worth trusting rather than treating as coincidence: a
session that opens one of those worktrees should start from the matching page
above instead of re-deriving scope. This page's own worktree
(`jdk-lanes-doc-retirement-*`) is why this lane's own groundwork below is
framed as doc retirement rather than a defect fix — see the last section.

Each is independently buildable and none shares a source prefix with another
(checked against `docs/internal/jdk-only/lane-0-integration-and-gates-RETIRED-20260916.md`
§2's table and against each other below). None needs the `RETIRED_SHADOW_L<N>`
retirement machinery except the last two, which extend an existing lane's own
tables rather than adding new ones — see those pages for the exact cells.

**Disjointness, stated once:** classloading+reflection touches
`jdk/internal/reflect/`, `sun/reflect/`, `java/lang/reflect/`,
`jdk/internal/loader/`, `java/lang/ClassLoader*`, and the class-identity /
hidden-class resolver in `vm/src/runtime/interpreter` + `native-api/src/class_identity.rs`
— none of it is `java/security/`, `java/util/concurrent/`,
`java/lang/invoke/`, `java/lang/StringBuilder`/`AbstractStringBuilder`, or
`java/nio/file/`, `sun/nio/fs/`, which is where the other four sit.

## What this lane's own work is

Groundwork that de-risks the other six without owning any of their prefixes:

1. **The kind-map baseline is stale by ~2000 rows** (lane-0 §"what it leaves
   open"). Re-freezing `scripts/baselines/jdk-only-kind-map-25-linux.tsv` needs
   a Linux census taken with the gate's own comparison tooling — the two
   commands lane-0 added on its last day score a Windows census against the
   committed baseline without a full re-derivation, and a future lane should
   use those rather than hand-deriving the freeze again.
2. **`BASELINE_INTRINSICS` is 1386 against a measured 1387**, deliberately not
   re-frozen because a missing `Intrinsic` reads two ways and naming which one
   needs two runs' per-file tables, not a source diff. Whoever does that
   diffing owns no code prefix while doing it — a groundwork task, not a lane's.
3. **Five `java/util/logging/` refusals carry an `Intrinsic` survivor** on the
   same slot campaign-wide (lane-0's own words: "deleting or retagging them is
   somebody's wave, not a defect here"). Deciding retag-vs-delete and doing it
   is small, cross-cutting, and belongs here rather than forcing a prefix lane
   to detour into logging.
4. **Criterion 5's one remaining gap**: `types/src/error.rs`'s
   `JdkOnlyViolation::render`/`to_json` have every field the acceptance
   criteria ask for except class ORIGIN
   ([`the-six-acceptance-criteria-measured-20260910.md`](the-six-acceptance-criteria-measured-20260910.md)
   §5). That doc already worked out the trap: six of fourteen construction
   sites fire at *registration*, before any class is loaded, so origin is
   structurally vacuous there — adding `origin: null` to tick a box would make
   the criterion read met while telling nobody anything. The fix is narrow: add
   origin only to the two class-facing variants, `CompatibilityClassRequested`
   and `NativeShadowsBytecode`. This is a diagnostics improvement every other
   lane's own probe debugging benefits from, which is why it sits here instead
   of being nobody's job.
5. **Keep this index current.** The six lane pages above will retire on their
   own schedule the way L0-L7/LT did; when one does, update its row here to
   point at the retirement rather than leaving a stale link — the exact mistake
   this page opened by describing.

## Doc retirement — done in this pass, and what is left

This worktree's own branch is named for doc retirement, which is the concrete
half of "groundwork" this pass actually executed rather than only scoped:

* **`INDEX.md` and `README.md` now carry a banner pointing forward** to this
  page and to `the-six-acceptance-criteria-measured-20260910.md`, instead of
  silently reading as current. Both were already five *known-stale* passes
  deep (INDEX.md's own "SECOND PASS"/"FOURTH PASS" banners, README's
  three-times-corrected headline count) — the fifth failure mode was letting a
  reader not notice the campaign they describe had finished at all, which no
  amount of recounting the old numbers would have caught.
* **What is deliberately NOT done here**: moving the ~350 dated records this
  directory accumulated since `INDEX.md`'s 2026-08-17 FOURTH PASS into
  `docs/internal/jdk-only/`, the way this directory's own earlier
  `RETIREMENT-*.md` audits retired closed work before. That is real,
  sizeable, mechanical work — reading and classifying on the order of 350
  files — and it is a fine next groundwork task for whoever next has this
  worktree's role, but rushing it into this pass would trade a careful split
  of the *remaining* work for a rushed sweep of the *finished* work. The
  banners above are what keep a reader from being misled in the meantime.

## What is deliberately excluded

**The "UNOWNED, frozen" 316 rows** lane-0 §2 named
(`jdk/internal/foreign/layout` leftovers, `java/beans`, `sun/java2d`,
`javax/management`, `sun/management`, `java/sql`, `java/awt/image`, `jdk/jfr`)
are frozen on purpose, not omitted by accident. None of the six lanes above
claims any of them. A lane that wants one claims it by amending
`docs/internal/jdk-only/lane-0-integration-and-gates-RETIRED-20260916.md` §2's
table in its own commit, per that page's own rule — do not fold one into a
lane below without doing that first.

**Retiring the reference discovery, FFM, SecurityManager, `System$1`, and
`ScheduledThreadPoolExecutor`-neighbouring keystore/HashSet/panama rows** that
lane 2's and lane T's own pages left blocked
(`lane-2-lang-values-RETIRED-20260911.md` §"Four of those arms", the lane T
retirement) are **not** reopened here. Each has a named, structural blocker
(no bytecode equivalent, the VM's own allocation shape, a security-model
decision) that a new lane would re-derive rather than solve. `ScheduledThreadPoolExecutor`
and the `StringBuilder` JIT door are the two exceptions — both have a
concrete, buildable next step, which is why they get lanes below instead of
staying frozen.
