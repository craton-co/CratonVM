# G54-1 — the two gates that disagreed: the ratchet re-taken and GREEN on a real run, `registrar_reachability.rs` found broken by an untracked scratch directory, and the `0/12` traced to a census that only looked in one crate

Status: **`G50-1` N2 CLOSED — the ratchet is re-taken (1,244 → 1,232 triples,
1,380 → 1,368 pairs, 111 → 110 passes) and `registrar_drift.rs` now passes
7/7. `G50-1` N5 CLOSED — the two gates contradicted each other because
`registrar_reachability.rs`'s `N/M triples` column was frozen prose from a
one-off census that counted only shipping registrars inside `native-builtins`;
drift was right, reachability's `0/12` was wrong, and the family's verdict is
now `TOMBSTONE`. `G41-1` N7 CLOSED. Both gates now cross-check each other in
both directions, verified by five mutations. And the caveat four lanes in a row
called their largest — "this file has never been compiled" — was never true:
`rustc --edition 2021 --test` builds and runs both gates in about a second with
no `cargo`.** Wave G, lane G54, 2026-08-17.

Files changed: `native-builtins/tests/registrar_drift.rs`,
`native-builtins/tests/registrar_reachability.rs`, and this record. Nothing
else — the `lib.rs`, `serialization.rs`, `phases_late/collections.rs` and
`native-io/src/lib.rs` halves are NOMINATIONS in §7.

---

## 0. What was and was not run

**The single most useful thing this lane did was type `rustc`.**

`cargo build`/`check`/`test` were out of scope for this lane, and four previous
lanes recorded "`cargo` was not available" as the reason `registrar_drift.rs`
had never been compiled. But neither gate depends on anything except `std` —
no `cratonvm-*` crate, no dev-dependency, no feature resolution. So a workspace
build was never needed:

```
CARGO_MANIFEST_DIR=C:/craton/CratonVM1/native-builtins \
  rustc --edition 2021 --test -O -o drift_gate.exe \
  native-builtins/tests/registrar_drift.rs
./drift_gate.exe --test-threads=1 --nocapture
```

Both files build this way in a few seconds and run in about one. `env!` picks
up `CARGO_MANIFEST_DIR` from the environment, and both scanners walk the real
working tree from there, so this is not a simulation of the gate — it *is* the
gate, the same code with the same inputs.

**`registrar_drift.rs` compiled clean on the first attempt.** Every risk
`G41-1` §10 listed in the order to check them — the match-ergonomic
destructuring in `retake`, `baseline_by_pass`'s `BTreeMap<&'static str,
BTreeSet<Triple>>` and its `entry(pass).or_default()`, the `&&String` bind in
the M12 `filter` — was fine. Not one of them was real. Three lanes' worth of
"assumed / not verified" resolved in one command.

What was run:

* **Both gates compiled and executed** against the working tree at `107efe18a`,
  before and after every edit in this record.
* **`retake()`'s paste-ready output, read for the first time by anyone** (§2.2).
* **Five mutations** against the two new cross-checks, each compiled and run
  (§4.4).
* **A source correlation** of `registrar_reachability.rs`'s 73 verdict strings
  against the drift census, per family, over each family's synthetic-only
  subtree (§3.3).

What was **not** run: no `cargo`, no VM, no registry dump, no regression
vectors. This lane changed only two test files, neither of which is compiled
into any shipping binary, and it had no source change whose runtime behaviour a
vector could measure. Every runtime claim quoted below is `G41-1`'s or
`G50-1`'s measurement, attributed as such.

Provenance tags: **MEASURED-BY-THE-GATE** (the Rust gate itself printed it),
**SOURCE-VERIFIED** (a human read the lines), **INHERITED** (a measurement from
`G41-1` or `G50-1`, taken on a binary this lane did not re-run),
**PREDICTED** (none of those).

---

## 1. The headline

| | before | after |
|---|---|---|
| `registrar_drift.rs` | never compiled; 4 pass / **2 fail** | **7 pass / 0 fail** |
| `registrar_reachability.rs` | never compiled; 1 pass / **3 fail** | **5 pass / 0 fail** |
| strict drift total | 1,244 (predicted 1,232) | **1,232, MEASURED-BY-THE-GATE** |
| `(pass, triple)` pairs | 1,380 | **1,368** |
| passes in `DRIFT_TRIPLES` | 111 | **110** |
| reachability: direct synthetic-only families | **54** observed vs 73 pinned | **73 vs 73** |
| reachability: synthetic-only closure | **169** observed vs 284 pinned | **285 vs 285** |
| do the two gates check each other? | **no — never once** | **yes, both directions** |
| `registrar_reachability.rs`'s `N/M triples` column | frozen prose, 43 of 73 wrong | **derived and machine-checked** |
| blind register sites (M12) | 950, ceiling 1,000 | **950, unchanged** |

---

## 2. Assignment A — the ratchet, re-taken and green

### 2.1 `G50-1` N2's instructions were right, and are now verified rather than trusted

The instruction was to verify rather than trust. Verified, and the verification
is stronger than the instruction asked for: rather than re-deriving the
arithmetic a third time by hand, the gate was compiled and run against the tree
with `G50-1`'s deletion already landed, and its own scanner was asked.

MEASURED-BY-THE-GATE, `107efe18a`:

```
registrar-drift: 361 files, 34197 fn defs, 849 passes,
  14086 register sites (12611 resolved, 696 loop-expanded),
  11458 distinct triples, 512 shipping-reachable,
  280 synthetic-only (73 direct), 1232 DRIFTING triples
```

**1,232.** `G50-1` §4 predicted 1,232 by arithmetic over the table. The Rust
resolver, run for the first time, produced 1,232 independently.

`retake()`'s regenerated table was then diffed against the hand-edited one. It
differed by **exactly** the `register_byte_array_output_stream` entry and
nothing else — 17 lines, no other row moved. So all three of `G50-1`'s edits
were correct and sufficient, and a full paste was not needed:

1. drop the `register_byte_array_output_stream` entry — done;
2. `BASELINE_TOTAL_PAIRS` 1,380 → 1,368, `BASELINE_TOTAL_DRIFT` 1,244 → 1,232 —
   done, and both now match what `retake` printed;
3. move all 12 rows from `MUST_DRIFT` to `FIXED_NOT_DRIFTING` — done, all
   twelve, so the vacuity half of `the_fixed_twins_stay_fixed` observes the
   whole family.

`RESOLVER_WITNESSES` and `SYNTHETIC_ONLY_CLOSURE` needed no change, as `G50-1`
said. The witness's *reason text* did: it read "a `let cls = \"...\"` binding",
naming a binding in a file that no longer has one. It now says it witnesses
native-io's `let baos = "…";` instead, which is the same resolution path — and
`the_fixed_twins_stay_fixed` passing is the proof the census still resolves it.

One correction to `G50-1` §4: it predicted the change would redden **three** of
the six tests. It reddened **two** — `the_drift_baseline_has_no_stale_rows` and
`the_known_live_twins_still_drift`. `the_baseline_is_well_formed` stayed green
because it checks the table against its own constants, and the table and the
constants were still consistent with each other while both were stale. That is
worth noticing rather than correcting silently: the well-formedness test cannot
detect a table that is internally consistent and collectively wrong, which is
exactly the state a half-finished re-take leaves behind.

### 2.2 The first-run experience, which nobody had seen

**What the first person to run
`cargo test -p cratonvm-native-builtins --test registrar_drift` should expect,
now:** all seven tests pass. The re-take is done. If it is red, this is the
order to read it.

1. **Read `the_drift_scanner_is_not_vacuous` first**, always. It prints the
   whole census in one line plus the unresolved-site breakdown. If `361 files`
   has collapsed, or `1232 DRIFTING` has become 40, the scanner is broken and
   every other failure below it is noise.
2. **Then `the_two_gates_agree_on_the_synthetic_only_population`** (new, §4.2).
   A shifted synthetic-only population moves every number in the file. If this
   is red, fix it before re-taking anything — a re-take taken against a broken
   population bakes the break into the baseline.
3. **Then the ratchets.** Both `no_new_mode_drift` and
   `the_drift_baseline_has_no_stale_rows` `println!` the complete replacement
   table immediately before asserting, between
   `// ======== PASTE-READY, regenerated from this run ========` and
   `// ======== END PASTE-READY ========`. `cargo test` prints a failing test's
   captured stdout, so it is already on screen. It replaces
   `BASELINE_TOTAL_DRIFT`, `BASELINE_TOTAL_PAIRS` and `DRIFT_TRIPLES`
   wholesale.

**The printer works.** That is not a small claim — it had never been executed,
and it is the entire justification `G41-1` §4.2 gave for reversing `G3-1`'s
argument and accepting a 1,900-line table ("a re-take is one paste, not a
re-derivation"). Run against the un-retaken table it emitted a well-formed
1,920-line block with correct constants at the top, and the diff against the
hand edit was empty. The bet paid.

Two things a first runner should know that the messages do not say:

* **A re-take is now a two-file operation.** Pasting `DRIFT_TRIPLES` without
  re-taking `FAMILY_DRIFT_EXPOSURE` in `registrar_reachability.rs` fails there
  instead — deliberately (§4.1). That gate prints its own paste-ready block.
* **`the_baseline_is_well_formed` going green proves nothing about staleness.**
  See §2.1.

### 2.3 The tree moves; the drift set does not

The scan at `107efe18a` saw 849 passes and 34,197 `fn` defs where `G41-1`'s
Python port saw 843 and 34,059, and 512 shipping-reachable passes where it saw
507 — four days and several parallel lanes of movement. **None of it moved the
drift set.** 1,232 is exactly 1,244 − 12, with the 12 being precisely the
family `G50-1` deleted.

That is the useful observation, and it is the answer to `G3-1` §5.2's objection
that an exact set ratchet "would redden the gate on any harmless resolver
improvement". Over the one interval anyone has now measured, the census was
markedly more stable than its own inputs.

---

## 3. Assignment B — why the two gates disagreed

### 3.1 `registrar_reachability.rs` was broken, and not in the way anyone expected

Before any of this lane's edits, the reachability gate was run for the first
time. **Three of its four tests were red.**

```
registrar-reachability: 171 crate files, 567 workspace files, 19358 fn defs,
  736 registration passes, 266 external refs,
  54 direct synthetic-only families, 169 synthetic-only passes,
  535 shipping-reachable

the_scanner_is_not_vacuous  FAILED:
  `register_synthetic_overrides` has only 54 synthetic-only direct children;
  the call scan is broken
no_new_synthetic_only_family  FAILED:
  19 allow-listed synthetic-only families are no longer synthetic-only
no_registrar_silently_orphaned_into_the_synthetic_arm  FAILED:
  115 pinned synthetic-only passes are no longer synthetic-only
```

It was claiming that 115 of 284 synthetic-only registrars — every
`register_phase55_natives` … `register_phase72_natives` and their whole
`p5x`/`p6x`/`p7x` subtrees — had been promoted onto the shipping path.

They had not. `register_phase55_natives`' only call site is
`native-builtins/src/lib.rs:24731`, inside `register_synthetic_overrides`
itself (which begins at `lib.rs:22259`). SOURCE-VERIFIED for all 19.

**The cause is `scan::rs_files`.** SECTION 3 step 5 walks the whole workspace
and treats *every* pass name mentioned outside `native-builtins/src` as a
SHIPPING ROOT. The walker skipped `.git`, `target`, `node_modules` and
`.claude` — the last with a comment saying exactly why ("Descending into one
would double every count and make the answer depend on which lanes happen to be
running"). It did not skip `scratch/` or `scratchpad/`, and on 2026-08-17 both
existed and were untracked: `scratch/` is git-ignored, `scratchpad/` shows as
`??` in `git status`. `scratch/` held
`SpringTestCompilerAnnotation-phases_late.rs`, a stray copy of `phases_late.rs`.

MEASURED-BY-THE-GATE, files contributing pass-name references to the shipping
roots, top of the list:

| file | refs | tracked? |
|---|---:|---|
| `vm/src/vm/tests.rs` | 4,003 | yes (a test module) |
| `scratch/SpringTestCompilerAnnotation-phases_late.rs` | 3,540 | **no** |
| `scratchpad/g22/basecrate/src/lib.rs` | 1,780 | **no** |
| `scratchpad/g22/base_lib.rs` | 1,780 | **no** |
| `native-collections/src/lib.rs` | 1,780 | yes |
| `native-io/src/lib.rs` | 1,319 | yes |
| `scratchpad/orig_net_phase_e.rs` | 360 | **no** |
| `scratchpad/o.rs` | 360 | **no** |
| `scratchpad/o2.rs` | 44 | **no** |

Excluding `scratch/` and `scratchpad/` alone:

```
external refs 266 -> 63
direct synthetic-only families 54 -> 73     (exactly the 73 pinned)
synthetic-only passes         169 -> 284    (exactly the 284 pinned)
ALL FOUR TESTS PASS
```

So the gate was correct as authored on 2026-08-13 and was broken purely by
untracked working directories that lanes create and delete. `scratch` and
`scratchpad` are now in the skip list, alongside `.claude`, for the reason
`.claude`'s comment already gave.

**The near-miss is the point.** All three failure messages recommend the same
repair — "Remove them from `DELIBERATE_SYNTHETIC_ONLY_FAMILIES` and from
`SYNTHETIC_ONLY_CLOSURE`", "Shrink `SYNTHETIC_ONLY_CLOSURE` in the same commit"
— and following that advice would have deleted two thirds of the population
this gate exists to watch, leaving it green and blind. The only thing standing
in the way was `the_scanner_is_not_vacuous`'s direct-child floor, which fires
first and says "the call scan is broken" rather than "good news". The floors
`G41-1` §7 declined to tighten because "a floor that tracks the measurement
exactly is a maintenance tax that gets relaxed under pressure" are the reason
this was caught instead of ratified.

### 3.2 A second, tracked hole: test *files* were shipping roots

The workspace walk skipped path *segments* named `tests`, `benches`, `examples`
and `fuzz`. `vm/src/vm/tests.rs` is a file, not a directory, so it walked
straight through — contributing more pass-name references than any other file
in the workspace (4,003). It is declared at `vm/src/vm.rs:59-60` as

```rust
#[cfg(all(test, feature = "synthetic-jdk"))]
mod tests;
```

— doubly not a shipping call site: a test, and a test compiled only under the
feature this gate exists to say the shipping binary does not have. The module
header already says why this matters: "A call site in a test is not a shipping
call site — treating one as such is precisely how a test comes to cover an
implementation the shipping mode never runs."

Closed: the walk now also skips files named `tests.rs` or `*_tests.rs`.
MEASURED-BY-THE-GATE effect, with `scratch`/`scratchpad` already excluded:
external refs 63 → 60, and **exactly one** pass moves into the synthetic-only
closure — `register_synthetic_socket_stubs`
(`native-builtins/src/phases_early.rs:18440`). Its only non-test caller is
`register_phase53_socket_stubs` at `:18427`, itself synthetic-only; its only
other reference in the tree is `vm/src/vm/tests.rs:582`. It was being called
shipping-reachable on the strength of one test line. It is now in
`SYNTHETIC_ONLY_CLOSURE`, with that reason recorded on the row.

### 3.3 The `0/12`: a census that only looked in one crate

`registrar_reachability.rs:145` said

> `SHIPPING TWIN: no class exclusive to it; 0/12 triples also registered by a shipping pass`

while `DRIFT_TRIPLES` listed all 12 as drifting, and `G50-1`'s two-mode registry
dump showed `native-io/src/lib.rs` owning every one of them.

**The first thing to say is structural: that number was never computed by
`registrar_reachability.rs`.** Nothing in the file reads it, recomputes it, or
asserts anything about it. It is prose inside a `&'static str`, taken by a
one-off external census on 2026-08-13, and the only test that touches those
strings checks that they are at least 40 characters long. A number that nothing
recomputes cannot go stale loudly — only silently.

**Which gate was wrong: reachability.** Drift's 12/12 is right, corroborated
three ways — `G50-1`'s dump (INHERITED: 13 `native-io` rows, `kind = bridge`,
`owns_slot = true`, identical in both modes, zero rows naming
`serialization.rs`), the drift gate's own Rust scanner run here, and
SOURCE-VERIFIED reading of `native-io/src/lib.rs:7058-7085`, which binds 13
triples — the 12 plus `write([B)V`. (Ten of the thirteen are single-line
`registry.register(baos, …)` calls; the three `toString` overloads are
rustfmt-wrapped across five lines each, which is why a naive one-line grep
finds only ten. Worth stating because it is exactly the kind of thing a
hand-run census gets wrong.)

**Now the generalisation, which is the part `G50-1` N5 asked for and did not
have.** All 73 verdicts were re-derived against the drift census, summing each
family's drifting triples over its synthetic-only subtree. 43 of the 73 differ.
But most of those differ in the *denominator* too — the two censuses disagree
about how many triples a family registers at all — so a raw 43 says little.

Restricting to the **31 families where both censuses agree exactly on the
denominator**, and therefore see the same registrations:

| | count |
|---|---:|
| agree exactly on the numerator | **26** |
| reachability **understates** | **5** |
| reachability overstates | **0** |

**Zero overstatements.** A one-sided error is the signature of a lookup that
misses, not of noise or drift-over-time. The five:

| family | recorded | measured | where the shipping twin lives |
|---|---:|---:|---|
| `register_byte_array_output_stream` | 0 | 12 | `native-io` |
| `register_completable_future_natives` | 0 | 3 | `native-collections` |
| `register_m18_concurrent_fixes` | 2 | 35 | `native-collections` (35) + `native-builtins` (2) |
| `register_http2_natives` | 38 | 48 | `native-builtins` (`net_phase_e.rs`) |
| `register_concurrent_extras` | 2 | 3 | `native-builtins` |

The first three settle it. `register_m18_concurrent_fixes` is the cleanest
possible witness: it drifts on 35 triples, **all 35** have a twin in
`native-collections/src/lib.rs`, exactly **2** of them also have a twin in
`native-builtins/src/reflect_annotations.rs` — and the recorded number is 2.
The census counted the `native-builtins` twins and nothing else.
`ByteArrayOutputStream` (12 in `native-io`, recorded 0) and
`CompletableFuture` (3 in `native-collections`, recorded 0) are the same rule
with no `native-builtins` twin at all to survive it.

So the hypothesis holds, with a caveat worth stating: **the column understated
every family whose twin lives outside `native-builtins`, and it was also four
days stale.** The remaining two understatements have `native-builtins` twins
and are ordinary staleness — `net_phase_e.rs` is named in `G41-1` §2 as one of
the files parallel lanes were editing mid-census.

This matters beyond one row. `G41-1` §6 measured which crate owns the shipping
twin across the whole drift set: `native-builtins` 904, `native-collections`
141, `native-io` 58, `vm` 1. Roughly one drifting triple in six has its twin
outside `native-builtins` — which is the entire reason `registrar_drift.rs`
scans seven crates and says so in its header ("A one-crate scan manufactures
false drift *and* misses real drift"). The reachability census was the
one-crate scan that header warns about.

**Disposition for `register_byte_array_output_stream`: `TOMBSTONE`, verified
rather than assumed.** `G50-1` suggested it; the criterion is "registers
nothing at all". SOURCE-VERIFIED:
`native-builtins/src/serialization.rs:5064` is
`pub(crate) fn register_byte_array_output_stream(_r: &mut NativeMethodRegistry) {}`
— an empty body, not merely a family whose triples all have twins.
MEASURED-BY-THE-GATE: the pass registers **0** triples in the census and is
absent from `DRIFT_TRIPLES` entirely. It is a tombstone by both tests. Note the
verdict genuinely changed *category*: while the pass still registered its 12,
`SHIPPING TWIN` was the right verdict with the wrong number (`12/12`, not
`0/12`); it became `TOMBSTONE` only when `G50-1` emptied it.

---

## 4. The two gates now cross-check each other

Neither gate read the other. That is why reachability could be broken in three
places while drift stayed green, and why drift's 12/12 could contradict
reachability's 0/12 for four months with nothing to notice.

### 4.1 Reachability → drift: `FAMILY_DRIFT_EXPOSURE`

The `N/M triples` clause is **gone from all 73 verdict strings**, replaced by a
pointer to a new table:

```rust
const FAMILY_DRIFT_EXPOSURE: &[(&str, usize)] = &[ … 73 rows … ];
```

`the_drift_gate_agrees_about_family_drift_exposure` reads
`tests/registrar_drift.rs` off disk, parses `DRIFT_TRIPLES` structurally, walks
each family's synthetic-only subtree using this gate's own call graph, and
asserts every number. On failure it prints a paste-ready replacement table, on
the model of `retake`.

Three deliberate choices:

* **The denominator `M` is deleted, not re-derived.** This gate has no triple
  resolver and cannot recompute `M`; keeping an un-recomputable number beside a
  recomputable one is precisely how the `0/12` survived. If the scale is
  wanted, the drift census is where it can be measured.
* **Zeros are pinned like any other value.** `0` is a real claim — the family
  drifts on nothing — and it is the claim that was false for
  `ByteArrayOutputStream`. Omitting zeros would have made the original bug
  unrepresentable rather than wrong.
* **The chain is one link longer than it looks, and the doc comment says so.**
  `DRIFT_TRIPLES` is a baseline, not a live measurement. What makes it
  trustworthy is that the drift gate's own two-sided ratchets pin it to the
  live tree; this table is then pinned to it. If those ratchets are red, this
  number is only as good as a stale table — so read that failure first.

### 4.2 Drift → reachability: `KNOWN_POPULATION_DIVERGENCE`

`the_two_gates_agree_on_the_synthetic_only_population` reads
`tests/registrar_reachability.rs`, parses both allow-lists, and asserts:

1. **The 73 direct families must match exactly.** MEASURED-BY-THE-GATE: they
   do, name for name. Two independently written scanners, different crate
   scopes (seven `src` trees vs one), different shipping-root rules, agreeing
   on all 73 is the strongest evidence either file has ever had that its
   reachability half is right — and a count could not deliver it, which is why
   `Analysis` now carries the direct children as a set rather than a `usize`.
2. **The transitive closures may differ only by an enumerated table.** They
   differ by exactly seven names, and in **every one of them the reachability
   gate is right and the drift gate over-reads `shipping`** (§4.3).

### 4.3 What the cross-check found immediately: drift over-reads `shipping` seven ways

| name | why they differ | who is right |
|---|---|---|
| `register_p60_callsite` | drift counts the `use` import at `phases_late.rs:49` as a shipping root | reachability |
| `register_p60_record` | `use` at `phases_late.rs:52` | reachability |
| `register_p65_method_handles_extra` | `use` at `phases_late.rs:49` | reachability |
| `register_phase52_string_buffer` | `use` at `phases_early.rs:47` | reachability |
| `register_phase53_record` | `use` at `phases_early.rs:44` | reachability |
| `register_synthetic_socket_stubs` | drift reads `vm/src/vm/tests.rs:582` | reachability |
| `register_synthetic_overrides` | structural: drift keeps the closure's root, reachability removes it | neither — by design |

The first five are one defect. `registrar_drift.rs` SECTION 3 has a
module-level scan whose comment reads "a pass named in a `static` table, a
`use` re-export — count as non-pass references too", and it therefore treats
`use crate::lang_misc::register_p60_record;` as a shipping root. **An import is
not a call.** SOURCE-VERIFIED: all five are called from exactly one place —
`register_phase52_natives`, `register_phase53_natives`,
`register_phase60_natives` (twice) or `register_phase65_natives` — every one of
which is itself a direct synthetic-only child of
`register_synthetic_overrides`. Their genuine call sites are already captured by
the in-function scan, so the `use` rule buys nothing and costs five false
shipping roots.

**Consequence: `BASELINE_TOTAL_DRIFT` is an UNDER-count.** Those five passes are
excluded from the synthetic-only set, so whatever they share with a shipping
pass is not counted as drift. This was **not** repaired here, deliberately:
fixing it moves the census, and a census move is a re-take, not a cross-check —
it would have entangled Assignment A's numbers with Assignment B's. It is
nomination **N1**, and until it lands the table is the honest statement of
where the two gates disagree. A sixth name on either side fails.

### 4.4 The cross-checks bite — five mutations, each compiled and run

A cross-check that passes vacuously is worse than none. MEASURED-BY-THE-GATE:

| # | mutation | result |
|---|---|---|
| M-A | one `FAMILY_DRIFT_EXPOSURE` number edited by hand (8 → 7) | **FAILS**, names the family |
| M-B | a pass entry deleted from `DRIFT_TRIPLES` without re-taking the exposure table | **FAILS**: "recorded 8, measured 0" |
| M-C | reachability's two allow-lists shrunk by 19 families — *the exact repair its own broken failure messages recommend* | **FAILS** on the direct-family set |
| M-E | a `KNOWN_POPULATION_DIVERGENCE` row gone stale (the two now agree) | **FAILS**: "the two gates now AGREE about them" |
| M-F | an unexplained closure divergence (`register_arc_container` dropped) | **FAILS**, with the pass's definition site |

**M-C is the one that matters.** It is the scratch-directory break of §3.1
followed by the tempting wrong fix, and it is now caught from the other side.
To be precise about what is and is not covered: the drift gate's cross-check
reads reachability's *pinned tables*, not its computed set, so it would **not**
have gone red the moment `scratch/` appeared — reachability's own three
ratchets do that. What it blocks is the second step, where someone silences
those three by shrinking the tables. That is the step that would have been
permanent, and it is now impossible without the drift gate objecting.

Both new tests carry their own non-vacuity floors, and one of them earned its
keep on its first execution: the `DRIFT_TRIPLES` parser anchored on the first
`&[` after the declaration name, which is the *type* `&[(&str, &[(&str, &str,
&str)])]`, not the value. It parsed 0 passes and would have agreed with
everything. The floor fired instead — "parsed only 0 passes / 0 distinct
triples … the 'confident, vacuous zero' this family has recorded three times" —
and the anchor is now `= &[`, with a comment saying why.

---

## 5. The 122 "neither copy registered" rows: **no, the gate cannot see them**

Asked plainly, answered plainly.

`G41-1` §6 measured 122 of the drift rows as triples where the shipping twin is
tagged `SyntheticStub`, which `native-api/src/registry.rs`'s `allowed_in`
refuses outright under `--jdk-only` — so **neither** copy is registered and real
JDK bytecode serves the call. `G50-1` §3 adjudicated one such family
(`java/time/Instant`, 16 triples) as MEASURED CORRECT. A parallel lane found 4
of 14 bypassed stackless triples are `SyntheticStub` Panama `DowncallHandle`
arms.

**Neither gate can distinguish these rows, and no cheap change makes them
able.** Kind is not a property of the `register()` call site. It is ambient
registry state, and this lane measured how ambient, which nobody had:

| how kind is set | call sites |
|---|---:|
| `set_category(...)` — imperative, flows across statements and *across function calls* | **1,202** |
| `with_category(kind, \|r\| { ... })` — lexically scoped closure | **42** |

SOURCE-VERIFIED across the seven scanned crates, excluding test trees. Only
**3.4%** of kind-setting is lexically recoverable. A scanner that resolved the
`with_category` form — the only form a source scan could handle — would cover
42 sites out of 1,244 drift rows and would then *look* like kind coverage. That
is worse than the current honest silence, and it is why this lane did not build
it.

What the gate can do, and now does, is refuse to pretend. The claim is stated
in three places in `registrar_drift.rs` (module header, `MAX_BLIND_SITES`, and
`no_new_mode_drift`'s doc comment) in one sentence: **"A row in this baseline is
a claim that two registrations exist. It is not a claim about which body runs."**
The only visibility that exists is the three `MUST_DRIFT` rows that carry dump
evidence in their reason text (`Instant.getEpochSecond`, `AtomicBoolean.get`,
`slf4j Logger.debug` — each confirmed `kind = synthetic-stub` in a
compatible-mode dump and ABSENT from the `--jdk-only` dump). Three of 122 are
pinned. The other 119 are invisible to both gates and always will be from
source.

The honest resolutions are unchanged from `G3-1` N6 / `G41-1` N8, and this
lane's measurement sharpens the choice: either make kind statically recoverable
(which means converting ~1,200 `set_category` sites to a scoped form — a real
project, not a lane), or accept permanently that `--dump-native-registry` is the
only admissible evidence for a kind claim and build the second instrument
`G50-1` N6 describes. **The 122 are a dump problem, not a gate problem.** Any
future record that implies a source gate will one day catch kind drift is
wrong.

---

## 6. The known holes, preserved not dropped

`G41-1` §5's M12 region is intact and unchanged. MEASURED-BY-THE-GATE at
`107efe18a`:

| reason | sites |
|---|---:|
| `unbound-identifier` | 892 |
| `format!` | 18 |
| `no-enclosing-fn` | 15 |
| `no-registry-owner` | 14 |
| `expression` | 11 |
| **total that could hide drift** | **950** of 14,086 |
| ceiling `MAX_BLIND_SITES` | 1,000 |
| `arity<4` (proven non-registry, excluded) | 499 |

Exactly the 950 `G41-1` measured with its Python port — the first confirmation
that the Rust and the port agree on this number too. Drift arriving through a
`format!`-built descriptor, a class-parameterised registrar, a tuple `for` loop
or array-const iteration remains invisible; the region is bounded and cannot
grow silently. Nothing in this lane widened or narrowed it.

Kind drift (M13) remains entirely invisible, now with §5's measurement of why.

---

## 7. NOMINATIONS

**N1 — `registrar_drift.rs`: stop treating a `use` import as a shipping root.**
This lane's own file, deliberately not done here. SECTION 3's module-level scan
inserts every pass name appearing outside a `fn` body into `non_pass_called`,
which seeds the shipping closure. A `static` table entry is a real reference; a
`use` item is not — the call sites it enables are already counted by the
in-function scan. MEASURED (§4.3): five passes are wrongly shipping because of
it, all five reachable only from a synthetic-only `register_phaseNN_natives`,
so `BASELINE_TOTAL_DRIFT` is an under-count by whatever they share with a
shipping pass. **This is a re-take, not a bug fix**: the census moves, the whole
table must be re-taken from `retake`'s output, and `FAMILY_DRIFT_EXPOSURE` with
it. It belongs in its own commit with `KNOWN_POPULATION_DIVERGENCE` shrunk by
five rows in the same change — `the_two_gates_agree_on_the_synthetic_only_population`
will demand exactly that and fail until it happens. A `pub use` re-export must
keep counting: it genuinely exposes a name to other crates.

**N2 — `registrar_drift.rs`: stop reading `vm/src/vm/tests.rs` as source.**
Same file, same reason for deferring. Its `rs_files` skips *directories* named
`tests`, so it reads that file, and the test call at `:582` makes
`register_synthetic_socket_stubs` shipping-reachable. `registrar_reachability.rs`
closed the identical hole in this lane (§3.2); the drift side was left because
it also moves the census. Land it with N1 — one re-take for both.

**N3 — `scratch/` and `scratchpad/` should be in `.gitignore` and in every
scanner's skip list.** Not this lane's files. `scratchpad/` is currently `??` in
`git status`, i.e. one `git add -A` from being committed, and it contains two
copies of a crate `lib.rs`. Both gates now skip both directories, but any future
tree-walking test will hit this again, and the failure mode is a confident wrong
number rather than an error. A one-line `.gitignore` entry and a note in
`CONTRIBUTING` would end the species.

**N4 — `native-io/src/lib.rs`: `new ByteArrayOutputStream(negative)` must
throw.** Carried forward unchanged from `G3-1` N3, `G41-1` N3 and `G50-1` N4,
and still the only place it can be fixed now that the `serialization.rs` copy
that clamped with `.max(1)` is gone. `native_baos_init_capacity` falls back to
32; HotSpot throws `IllegalArgumentException: Negative initial size: -1`. Not
drift — a defect the surviving copy has on its own. This is now the oldest
untaken nomination in the family.

**N5 — `native-builtins/src/lib.rs`: delete the two calls to
`register_byte_array_output_stream`, and rename
`register_synthetic_instant_stub_natives`.** Both carried forward from `G50-1`
N1 and N3 unchanged. The BAOS function is empty and `TOMBSTONE`-verdicted; the
calls are harmless but the empty function is an invitation to refill it. The
`Instant` rename matters more: a pass on the SHIPPING path
(`reflect_annotations.rs:758` → `register_essential_natives_with_shims`) is
called `register_synthetic_*`, and `G3-1` N4 asked for the rename two days ago.

**N6 — the `--jdk-only-report` needs a third disposition.** Carried forward from
`G50-1` N6, and §5 strengthens the case: 1,341 `synthetic-native-registered`
violations in one run, a tag whose remedy line says "implement or re-tag", when
`G50-1` measured that for `java.time.Instant` the correct remedy is a third
thing — delete the registration, the JDK is already right. A pass over the 122
rows asking `Instant`'s question of each is the cheapest large reduction
available, and it is the only instrument that can see the rows §5 says no source
gate ever will.

**N7 — carried forward untouched:** `G41-1` N5 (the 935 closure-vs-closure
rows), N6 (the 140 dump-absent triples). `G41-1` N7 (reachability should adopt
the brace-balance self-check) is **superseded**: the concrete defect it was
worried about turned out to be the file walk, not the brace scan, and the
cross-check in §4.2 catches the class of failure it was aiming at from a better
angle.

---

## 8. Verified vs assumed

**Verified.**

* That both gates compile under `rustc --edition 2021 --test` and that
  `registrar_drift.rs` compiles clean on the first attempt, with none of
  `G41-1` §10's three predicted risk points real.
* Every number in §1, §2.1, §2.3, §3.1, §3.2, §3.3, §4.3, §4.4 and §6 —
  printed by one of the two gates, or by an instrumented copy of one, run
  against the working tree at `107efe18a`.
* That `retake()`'s regenerated table differs from the hand-edited table by
  exactly the `register_byte_array_output_stream` entry — full text diff.
* That the two gates agree name-for-name on all 73 direct synthetic-only
  families, and that their transitive closures differ by exactly seven names.
* That all five `use`-import passes and `register_synthetic_socket_stubs` are
  called only from synthetic-only passes — `grep` across all crates excluding
  `target/` and `scratch/`, with the enclosing function of every call site
  resolved by line number.
* That `native-io/src/lib.rs` binds 13 `ByteArrayOutputStream` triples
  including `write([B)V`, and that
  `serialization.rs:5064`'s `register_byte_array_output_stream` has an empty
  body.
* The five mutations in §4.4, each compiled and run.
* That both files are LF-only (0 CR bytes) and `rustfmt --edition 2021 --check`
  CLEAN — as was `HEAD` for both, so **no new hunks and in fact no hunks at
  all**. One new hunk was introduced by this lane's `FIXED_NOT_DRIFTING` edit
  and removed before finishing.
* `set_category` 1,202 vs `with_category` 42 (§5).

**Assumed / not verified.**

* **That `cargo test -p cratonvm-native-builtins --test registrar_drift` gives
  the same result as the standalone `rustc` binary.** It should — same source,
  same edition, same `CARGO_MANIFEST_DIR`, no dev-dependencies in play — but
  `cargo` was not run and the difference, if any, would be in the harness
  rather than the gate.
* **That `FAMILY_DRIFT_EXPOSURE`'s 73 numbers are "correct"** in any sense
  beyond "reproducible from `DRIFT_TRIPLES` and this gate's call graph". They
  are *derived*, which is the property that was missing before; they inherit
  every limitation of the drift census, including the under-count N1 describes.
  73 families attribute 1,061 of the 1,232 drifting triples; the remainder is
  mostly the 173 that `register_synthetic_overrides` registers in its own body,
  which belongs to no family.
* **That the deleted `N/M` denominators were not carrying information worth
  keeping.** They were stale (`register_phase68_natives` said 372 where the
  drift census sees 88) and un-recomputable here, but nobody re-measured them
  before they were dropped.
* Every runtime claim about registry contents, `kind`, `owns_slot` and
  `invocations` is INHERITED from `G41-1` and `G50-1`. This lane ran no VM.
* The gates' runtime cost under `cargo`. Standalone they are ~1s each.

---

## 9. What this lane did NOT do

1. **It did not run `cargo`, a VM, a registry dump, or any regression vector.**
   It changed two test files, neither compiled into any shipping binary.
2. **It did not repair the drift gate's two shipping-root over-reads** (N1, N2),
   though it found, measured and pinned them. Both move the census and belong in
   a re-take commit.
3. **It did not touch `native-builtins/src/lib.rs`,
   `native-builtins/src/serialization.rs`, `phases_late/collections.rs`,
   `native-io/src/lib.rs`, `INDEX.md` or `README.md`** — nominations only.
4. **It did not make either gate able to see kind** (§5), and argues in §5 that
   no source gate should try.
5. **It did not re-derive the `M` denominators** it deleted from the 73 verdict
   strings; it removed them as un-recomputable rather than replacing them.
6. **It did not check the 119 unpinned "neither copy" rows** — it only measured
   why neither gate can.
