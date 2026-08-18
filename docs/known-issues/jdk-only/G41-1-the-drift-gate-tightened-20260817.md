# G41-1 — the drift gate tightened from a count to a set, the TreeMap twin closed on dump evidence, and the first registry-dump check of any number in this family

Status: **`registrar_drift.rs`'s ratchet is now a two-sided SET ratchet over
1,244 triples (was a one-sided per-pass COUNT ceiling over 1,266); M11 closed and
demonstrated closed by mutation; M12 bounded; M13 measured for the first time and
still not closed. The 24 `TreeMap`/`TreeSet`/`NavigableMap`/`NavigableSet`
navigation triples are no longer drifting — `register_p62_navigable_expansion`'s
arms and their dead bodies are deleted, on registry-dump evidence taken in both
modes.** Wave G, lane G41, 2026-08-17. Picks up `G3-1` N1 and N7.

Files changed: `native-builtins/tests/registrar_drift.rs`,
`native-builtins/src/phases_late/collections.rs`, and this record. Nothing else.

---

## 0. What was and was not run

**`cargo` was not available to this lane either.** `registrar_drift.rs` has
*still* never been compiled. That is the third lane in a row to say so, and it is
the single largest caveat on everything below.

What *was* run, which is new:

* **`cratonvm --dump-native-registry`, in both modes**, from
  `C:/craton/target-rel2/release/cratonvm.exe` (`9964ca733`; `target-rel3` does
  not exist on this machine). 12,039 registry rows in compatible mode, 10,691
  under `--jdk-only`. This is the first time any number in the `F34-1`/`G3-1`
  family has been checked against a running VM.
* **A line-by-line transliteration of `registrar_drift.rs`'s own scanner into
  Python**, run over this working tree. Not a second reading of the spec — a
  port of SECTION 2 and SECTION 3 of that file, function by function, in order.
* **A simulation of every assertion in the tightened gate** against that port's
  output, and an M11 mutation run through both the old and the new ratchet.
* **The six required regression vectors**, on CratonVM in compatible mode and
  under `--jdk-only`, against a HotSpot 25 oracle.

Provenance tags: **MEASURED-ON-BINARY** (a registry dump or a VM run says it),
**MEASURED-ON-SOURCE** (the transliterated scanner counted it),
**SOURCE-VERIFIED** (a human read the lines), **PREDICTED** (none of those).

---

## 1. The headline

| | `G3-1` (2026-08-16) | this lane (2026-08-17) |
|---|---|---|
| ratchet shape | per-pass **count**, one-sided ceiling | per-`(pass, triple)` **set**, two-sided |
| baseline | 112 rows + a global total of 1,266 | 111 passes / **1,380 pairs / 1,244 triples** |
| M11 — swap at constant count | **catches NOTHING** | **catches it** (§4) |
| M12 — `format!` / class-parameterised / tuple-loop blind spot | invisible, unbounded | invisible, **bounded** at 950 sites (§5) |
| M13 — kind drift | invisible, unquantified | invisible, **quantified**: ≥122 rows (§6) |
| `TreeMap`/`TreeSet` navigation drift | 24 triples, LIVE | **0 — closed** (§3) |
| numbers checked against a running VM | none | 1,104 of 1,244 (§6) |

The gate now has six tests, not four:
`the_drift_scanner_is_not_vacuous`, `the_baseline_is_well_formed`,
`no_new_mode_drift`, **`the_drift_baseline_has_no_stale_rows`**,
`the_known_live_twins_still_drift`, **`the_fixed_twins_stay_fixed`**.

---

## 2. The transliteration, and why its numbers can be trusted more than the last set — MEASURED-ON-SOURCE

`G3-1` §9 warned its successor in as many words: *"If the first run is red on
`no_new_mode_drift`, re-take the baseline from the gate's own printed output
before assuming a regression; the baseline came from a Python mirror."* A mirror
written from the same spec by the same hand in the same sitting is the weakest
form of agreement there is, and it said so.

This lane did not write a mirror. It **ported the Rust file**: `blank`,
`raw_string_open`, `match_brace`/`match_paren`/`match_bracket`, `word_at`,
`skip_ws`, `rs_files`, `split_args`, `unescape`, `as_literal`,
`strip_adaptors`, `is_plain_ident`, `path_tail`, `parse_file`, `str_consts`,
`resolve_simple`, `let_bindings`, `parse_loops` and `build_analysis`, each
transliterated in place with the same off-by-one behaviour, the same
first-binding-wins `let` rule, the same `.`-exclusion in the call graph, and the
same `envs.len() <= 512` loop cap.

Its output against `G3-1`'s independent mirror, on a tree five commits further
on:

| | mirror, 2026-08-16 | this port, 2026-08-17 (pre-fix) |
|---|---:|---:|
| `.rs` files | 361 | **361** |
| `fn` definitions | 33,710 | 34,059 |
| registration passes (distinct) | 838 | 843 |
| `.register(` sites | 14,063 | 14,088–14,110¹ |
| …resolved | 12,600 | 12,647 |
| …loop-expanded | 695 | **696** |
| distinct triples | 11,422 | 11,458 |
| shipping-reachable passes | 521 | 507 |
| synthetic-only passes | **280** | **280** |
| direct synthetic-only children | **73** | **73** |
| `register_synthetic_overrides` body | 104,360 B | 104,469 B |
| baseline rows | **112** | **112** |
| **drifting triples** | **1,266** | **1,268** |

¹ the tree moved *during this lane*: `native-io/src/lib.rs`,
`native-builtins/src/net_phase_e.rs`, `vm/src/jit/helpers.rs` and
`vm/src/runtime/interpreter/invoke.rs` all gained edits from parallel lanes
between two runs minutes apart. That is the reason §7 explains why the ratchet is
a set with a self-repairing failure mode rather than an equality on counts.

Two independent implementations, two days apart, different authors, agreeing to
2 triples out of 1,266 across a moving tree. The remaining caveat is unchanged
and load-bearing: **neither of them is the Rust code that will actually run.**

---

## 3. `G3-1` N1 closed: the `TreeMap` navigation twin

### 3.1 What was there — SOURCE-VERIFIED

`register_p62_navigable_expansion`
(`native-builtins/src/phases_late/collections.rs:374`, reachable only from
`register_synthetic_overrides`) registered exactly 24 triples:

* `java/util/TreeMap` — `floor/ceiling/lower/higher` × `Key`/`Entry`, 8;
* `java/util/NavigableMap` — the same 8, bound to the same `p62_tm_*` bodies;
* `java/util/TreeSet` — `floor/ceiling/lower/higher`, 4;
* `java/util/NavigableSet` — the same 4, bound to the same `p62_ts_*` bodies.

No fifth class, no `for` loop, no `format!`. The bodies were linear scans over
the interleaved `[k0,v0,k1,v1,…]` slot-0 array through a
`natural_compare_values` helper that knew only `Int`/`Long`/`Float`/`Double` and
**returned `0` for every other pair of values** — so any non-primitive key
compared equal to every other, and a user `Comparator` was not consulted at all.

The shipping twin (`native-collections/src/lib.rs`,
`register_tree_map_natives` / `register_tree_set_natives` → `native_tm_*` /
`native_ts_*`) calls `tm_sync_native_state` first, has a `tm_fast_with` BTree
range path, honours a user `Comparator`, and re-reads `data` after a comparator
call because that call can move the heap ("Family-1 stale-`ObjectRef` fix").

### 3.2 `F34-1` §5's trap, checked against the dump and not against the source — MEASURED-ON-BINARY

`G3-1` N1 said the delete "looks safe — PREDICTED, verify with
`--dump-native-registry`". Verified:

```
cratonvm --dump-native-registry <file> -cp build RJdkCollections      # mode "compatible"
cratonvm --jdk-only --dump-native-registry <file> -cp build RCollections
```

All 24 triples, in **both** dumps, identical rows:

| class | methods | `kind` | `owns_slot` | `overwrote` | `registered_by` |
|---|---|---|---|---|---|
| `java/util/TreeMap` | 8 | `bridge` | `true` | `null` | `native-collections/src/lib.rs:48609–48651` |
| `java/util/NavigableMap` | 8 | `bridge` | `true` | `null` | `…:48821–48863` |
| `java/util/TreeSet` | 4 | `bridge` | `true` | `null` | `…:48980–48998` |
| `java/util/NavigableSet` | 4 | `bridge` | `true` | `null` | `…:49116–49134` |

and the `--jdk-only` dump's whole-registry `synthetic-stub` count is **0**, so
nothing here is being refused by `allowed_in`. `G34-1` settled that registering
a `Bridge` is by itself enough to preempt real JDK bytecode.

**The interface rows are the point.** §5's trap is that deleting a synthetic-only
pass can take away triples its twin never registered — and the plausible victims
here were `NavigableMap`/`NavigableSet`, since a `register_tree_map_natives`
might reasonably have bound only the concrete class. It binds both. The
synthetic-only arms were a strict subset, 24 of 24.

`invocations` was `0` on all 24 rows and is **not** used as evidence anywhere
above: the counter misses intrinsic-cached and JIT direct-call dispatch and is
exact only under `--nojit` with `CRATONVM_DISABLE_INTRINSICS=1`. `owns_slot` is
what was read.

### 3.3 What was changed

`register_p62_navigable_expansion` is now an empty, heavily documented function,
and the 13 dead bodies (`p62_tm_floor_key` … `p62_ts_higher`, `p62_tm_entry_at`)
plus `natural_compare_values` were **deleted**, not left behind: 408 lines gone.
Leaving them would have left a family of plausible-looking, unreachable
`TreeMap` natives for the next lane to "fix" — and *this branch has already
shipped a fix into dead code once* (`8c72d23ca`).

The function itself is kept because its only caller is
`native-builtins/src/phases_late.rs:3934`, which this lane does not own. Deleting
the call site is nomination **N1a** below.

Every identifier the deleted bodies used (`obj_arg`, `p64_make_entry`,
`ObjectRef`, `MethodCallFailed`, `get_array_element`) has other users in the same
file, and the file imports via `use super::*`, so no import is orphaned.
`natural_compare_values` and the `p62_*` symbols have **no** referent anywhere in
the workspace outside this file — checked by `grep` across all seven crates plus
`vm-cli`, excluding `target/` and the `scratch/` copies.

### 3.4 It is pinned, with the vacuity guard

The 24 are now `FIXED_NOT_DRIFTING` in the gate, checked by
`the_fixed_twins_stay_fixed` on **both** halves:

* each must be **absent from the drift set** — the fix held;
* each must be **present in the census** — the fix is being *observed*. Without
  this half the test passes the moment the resolver loses the triple, which is
  the "confident, vacuous zero" `F34-1` §2.1 recorded twice.

---

## 4. `G3-1` N7 closed: the ratchet is a set, and M11 is demonstrably shut

`DRIFT_BASELINE: &[(&str, usize)]` is gone. In its place:

```rust
const DRIFT_TRIPLES: &[(&str, &[(&str, &str, &str)])] = &[ … ];   // 111 passes
const BASELINE_TOTAL_PAIRS: usize = 1_380;                        // (pass, triple) pairs
const BASELINE_TOTAL_DRIFT: usize = 1_244;                        // distinct triples
```

* **`no_new_mode_drift`** — every currently-drifting `(synthetic-only pass,
  triple)` pair must appear in the table. A pass absent from the table has an
  allowance of zero.
* **`the_drift_baseline_has_no_stale_rows`** — every pair in the table must still
  drift. This is the second side `G3-1` §5.3 deliberately left off, and the
  reason it left it off (never compiled, an equality would be a coin flip) is
  answered by §7 rather than by staying one-sided.

Two derived numbers replace two independently-maintained ones: the per-pass
counts are now *derived from the set*, so the failure mode where a table and a
total are re-taken at different times cannot occur. `the_baseline_is_well_formed`
additionally refuses a table that contradicts the controls, `MUST_DRIFT`, or
`FIXED_NOT_DRIFTING`.

### 4.1 The mutation, run rather than imagined — MEASURED-ON-SOURCE

`G3-1` §5.2 marked its mutation column PREDICTED because its mirror computed the
census but did not re-implement the assertions. This lane's does. M11, applied to
`register_p63_resource_bundle` (9 drifting triples): remove
`ResourceBundle.containsKey(Ljava/lang/String;)Z` from its drift set and add a
substitute, leaving the count at 9.

```
OLD (count ceiling): baseline 9, now 9 -> PASSES (M11 hole)
NEW (set ratchet):   1 new pair, 1 stale pair -> FAILS
```

The other mutations from `G3-1` §5.2 are unchanged in effect except:

| # | mutation | 2026-08-16 | now |
|---|---|---|---|
| M7 | the `panama.rs` `structLayout` twin restored | NEGATIVE control | unchanged |
| M8 | the `p62_*` navigable family restored | POSITIVE control + `MUST_DRIFT` | **`the_fixed_twins_stay_fixed`** — the family is now the *negative* case |
| M11 | swap at constant count inside one pass | **NOTHING** | **`no_new_mode_drift` + `the_drift_baseline_has_no_stale_rows`** |
| M12 | drift through a `format!` descriptor etc. | NOTHING | still nothing; the **region is bounded** (§5) |
| M13 | same body, different `NativeKind` | NOTHING | still nothing (§6) |
| — | `per_pass` and `drift` edited apart | — | new self-check in `the_drift_scanner_is_not_vacuous` |

The positive control moved from `TreeMap.ceilingEntry` (now fixed) to
`ClassLoader.loadClass(Ljava/lang/String;)Ljava/lang/Class;` — three
synthetic-only registrants against the shipping
`register_classloader_real_natives`, confirmed `kind = bridge`,
`owns_slot = true`, `native-builtins/src/classloader_real.rs:841` in both dumps,
and in a family no open nomination proposes to touch.

### 4.2 What it cost, stated because `G3-1` argued the other way

`G3-1` §5.2 rejected exactly this trade: *"pinning all 1,540 triples exactly
would catch it and would also redden the gate on any harmless resolver
improvement — and a gate that reddens for no reason gets relaxed."*

The reversal is deliberate and the cost is real: **the table is 1,935 lines**,
and it will need re-taking whenever the resolver legitimately improves. What
makes the trade defensible now and not then is `retake` (§7): a re-take is one
paste, not a re-derivation. A gate that cannot see a swap is not measuring the
thing its name claims.

---

## 5. M12: the blind spot is still blind, but it can no longer grow

New in `the_drift_scanner_is_not_vacuous`:

```rust
const MAX_BLIND_SITES: usize = 1_000;   // measured 950
```

counting every unresolved register site **except** `arity<4` — the 499 sites
`G3-1` §3 classified by receiver as real `register` methods on the wrong
registry. MEASURED-ON-SOURCE, 2026-08-17:

| reason | sites |
|---|---:|
| `unbound-identifier` | 892 |
| `format!` | 18 |
| `no-enclosing-fn` | 15 |
| `no-registry-owner` | 14 |
| `expression` | 11 |
| **total that could hide drift** | **950** of 14,088 (6.7%) |
| `arity<4` (proven non-registry) | 499 |

**This does not make the drift in those sites visible.** New drift arriving
through a `format!`-built descriptor, a class-parameterised registrar, a tuple
`for` loop or an array-const iteration is still invisible to
`no_new_mode_drift`. What the ceiling adds is that the *region where it can hide*
cannot grow silently: a change that starts building descriptors with `format!`,
or moves a family behind `fn register_x(r, class: &str)`, now fails as a loss of
coverage. That is strictly weaker than seeing the drift, and the constant's
doc comment says so in those words.

(`G3-1` §3 quoted 383 unresolved sites at 2.7%. That was the *full census*, which
resolves class-parameterised registrars; the gate deliberately implements a
narrower resolver — see `G3-1` §6 — so its blind region is larger. Both numbers
are correct for their scanner.)

---

## 6. M13: kind drift, measured for the first time and still not closed

`G3-1` §3.1 predicted, from `native-api/src/registry.rs`, that a shipping twin
tagged `SyntheticStub` is *refused outright* under `--jdk-only`, so for such a
triple **neither** copy is registered and real JDK bytecode serves the call. It
could not measure it. MEASURED-ON-BINARY, over the 1,244 post-fix drifting
triples:

| | count |
|---|---:|
| present in the compatible-mode dump | 1,104 |
| …`kind = bridge` | 880 |
| …`kind = intrinsic` | 99 |
| …**`kind = synthetic-stub`** | **125** |
| present in the `--jdk-only` dump | 982 |
| synthetic-stub rows **absent** from the `--jdk-only` dump | **122 of 125** |
| absent from the compatible dump entirely | 140 |

Which slot-owning crate answers, compatible mode: `native-builtins` 904,
`native-collections` 141, `native-io` 58, `vm` 1.

So **at least 122 of the 1,244 recorded drift rows are rows where `--jdk-only`
runs neither implementation.** `G3-1`'s two worked examples both reproduce
exactly:

* `java/time/Instant.getEpochSecond()J` — `synthetic-stub` at
  `native-builtins/src/lib.rs:41202` in compatible mode, **absent** under
  `--jdk-only`;
* `java/util/concurrent/atomic/AtomicBoolean.get()Z` — `synthetic-stub` at
  `native-builtins/src/lib.rs:8521`, **absent** under `--jdk-only`, and the two
  registrations name the *same* callback. The "benign by identity" row is not
  benign.

A third, not previously measured: `org/slf4j/Logger.debug(Ljava/lang/String;)V`
— `synthetic-stub` at `native-builtins/src/logging_shims.rs:2567`, absent under
`--jdk-only`.

All three are now pinned in `MUST_DRIFT` with the dump evidence in the row.

**The gate still cannot see kind.** Kind is ambient `set_category` /
`with_category` state threaded through call chains; no source scan resolves it,
and this one does not try. The 140 triples the source scan reports as drifting
but which are absent from the compatible dump are the mirror-image caveat: the
source scan over-reads somewhere. Both directions are the reason the gate's own
doc comment now says, in `no_new_mode_drift`:

> A row in this baseline is a claim that two registrations exist. It is not a
> claim about which body runs.

---

## 7. Why the first run may still be red, and how it repairs itself in one step

`G3-1` §9's warning is inherited in full and cannot be discharged without
`cargo`. Two things make the first run cheap:

1. **`retake(&Analysis) -> String`** regenerates `DRIFT_TRIPLES`,
   `BASELINE_TOTAL_DRIFT` and `BASELINE_TOTAL_PAIRS` from what that run measured,
   in paste-ready Rust between two markers, and both ratchet tests `println!` it
   immediately before asserting. `cargo test` prints a failing test's captured
   stdout, so the replacement table is in front of whoever reads the failure
   without a 2,000-line panic message.
2. **The failure messages say how to tell the two causes apart.** A handful of
   unexpected rows is the transliteration disagreeing with the Rust resolver, and
   the right move is to re-take. Hundreds of rows is not, and the message says to
   read `the_drift_scanner_is_not_vacuous` first — its floors are what separate
   "the scanner broke" from "the tree changed".

The floors were **not** tightened onto the new measurements. They are still
generous (files ≥ 250 against 361, drift ≥ 900 against 1,244), for the reason the
2026-08-16 comment gives: a floor that tracks the measurement exactly is a
maintenance tax that gets relaxed under pressure. The *set* is the exact part.

### 7.1 Simulated result — MEASURED-ON-SOURCE

Every assertion in the tightened gate was re-implemented against the port's
output and run:

```
parsed DRIFT_TRIPLES: 111 passes, 1380 pairs, 1244 distinct
FIXED_NOT_DRIFTING rows: 24     MUST_DRIFT rows: 4
blind sites: 950  ceiling 1000
ALL SIMULATED ASSERTIONS PASS
```

This is a simulation of the assertions, not a run of them. It proves the tables
are internally consistent and consistent with the port; it does not prove the
Rust compiles.

---

## 8. Regression vectors — MEASURED-ON-BINARY

All six, CratonVM against a HotSpot 25 oracle, stdout diffed byte for byte.
`RTreeRangeGc` was given `--Xmx 64m` per `regression-suite/harness-guard.sh`'s
`class_cv_args` (without it the vector is inert).

| vector | compatible | vs HotSpot | `--jdk-only` | vs HotSpot | result |
|---|---|---|---|---|---|
| `RJdkCollections` | exit 0 | identical | exit 0 | identical | `PASS RJdkCollections (69 checks)` |
| `RCollections` | exit 0 | identical | exit 0 | identical | `PASS RCollections (53 checks)` |
| `RJdkViews` | exit 0 | identical | exit 0 | identical | `PASS RJdkViews (123 checks)` |
| `RJdkMapViews` | exit 0 | identical | exit 0 | identical | `PASS RJdkMapViews (74 checks)` |
| `RChmKeySetView` | exit 0 | identical | exit 0 | identical | `PASS RChmKeySetView` |
| `RTreeRangeGc` | exit 0 | identical | exit 0 | identical | `PASS RTreeRangeGc (14014 checks)` |

**What this does and does not prove.** The binary predates this lane's source
change and cannot contain it. What these runs measure is the *shipping* bodies —
`native_tm_*` / `native_ts_*` — which are precisely the bodies the change makes
the only copy in every mode. So they are a check that the surviving
implementation is the good one, not a check that the deletion compiles. The
deletion's effect on a `--features synthetic-jdk` build is **PREDICTED**: nobody
in this lane could build one.

---

## 9. NOMINATIONS

**N1a — delete the call to `register_p62_navigable_expansion`.**
`native-builtins/src/phases_late.rs:3934`. The function is now empty and
documented as such; the call is harmless but the empty function is dead weight
and an invitation to refill it. Deleting the call site and then the function is a
one-line change in a file this lane does not own. `the_fixed_twins_stay_fixed`
holds either way.

**N2 — `ByteArrayOutputStream.close()` / `flush()`: drop the synthetic no-ops.**
`native-builtins/src/serialization.rs`, the `r.register(cls, "flush", …)` /
`r.register(cls, "close", …)` pair at the end of
`register_byte_array_output_stream` (12 of 12 of that pass's triples drift).
They register no-ops; `native-io/src/lib.rs:6897`–`6898` bind `native_baos_close`
/ `native_baos_flush`, which dispatch `BaosEvent::Close` and run
`process_pipe_output_close`. **Now MEASURED, which `G3-1` could not do:** the
shipping bodies own the slot in *both* modes — `kind = bridge`,
`owns_slot = true`, `overwrote = null`, `native-io/src/lib.rs:6897` and `:6898`
in the compatible dump and in the `--jdk-only` dump alike. So the delete is safe
by the same argument §3.2 makes for `p62`, and costs nothing. **This is the
highest-value remaining LIVE drift row.** Neither file is this lane's.

**N3 — `new ByteArrayOutputStream(negative)` must throw.** Unchanged from
`G3-1` N3: `serialization.rs` clamps with `.max(1)`, `native-io/src/lib.rs:3822`
(`native_baos_init_capacity`) falls back to 32, HotSpot throws
`IllegalArgumentException: Negative initial size: …`. Not drift — a defect both
copies share. Fix belongs in `native_baos_init_capacity` and should land with N2.

**N4 — `java/time/Instant`, 16 triples: `--jdk-only` runs NEITHER copy.**
CONFIRMED ON A DUMP (§6): the shipping twin
(`register_synthetic_instant_stub_natives`, `native-builtins/src/lib.rs:41174`,
called from `reflect_annotations.rs:758`) sets `NativeKind::SyntheticStub`, and
`getEpochSecond()J` is present as `synthetic-stub` in the compatible dump and
**absent** from the `--jdk-only` dump. This is not only a naming problem: it is
16 `java.time` methods where strict mode falls through to real JDK bytecode,
which may well be *correct* — but nobody has said so deliberately. **Two
changes:** (a) rename the pass so a shipping registrar is not called
`register_synthetic_*`; (b) adjudicate whether `Instant` should be a `Bridge`
under `--jdk-only` or whether falling through is the intent, and record which.

**N5 — the 935 closure-vs-closure drifting rows are still undecidable by
reading, but they are no longer undecidable.** `G3-1` N8 asked for a differential
under `--dump-native-registry`. This lane has shown the instrument works and how
to read it: `registered_by` gives the owning `file:line`, `owns_slot` says who
holds the slot, and the `kind` column decides `--jdk-only` admission. The method
that produced §6 — join the source census's drift set against a dump keyed on
`(class, name, descriptor)` — resolves 1,104 of 1,244 rows to a concrete owning
body in seconds and needs no synthetic build. `invocations` remains unusable
without `--nojit` and `CRATONVM_DISABLE_INTRINSICS=1`. This is the largest
remaining unexamined surface.

**N6 — 140 drifting triples are absent from the registry dump entirely.** New,
and this lane did not chase it. The source scan says a pass registers them; the
compatible-mode registry has no such row. Either the resolver over-reads (a `let`
binding it resolved to the wrong literal, a loop it expanded too far), or the
registration is behind a `#[cfg]` this scan ignores, or a later `register()`
replaced the triple under a different key. Whichever it is, it is a **measured
upper bound on the census's precision** and the first one this family has ever
had. Worth a lane.

**N7 — `registrar_reachability.rs` should adopt the brace-balance self-check.**
Carried forward unchanged from `G3-1` N5; still not done, still two lines.

**N8 — `NativeKind` cannot be recovered from source, and the gate now says so
in three places.** Carried forward from `G3-1` N6. §6 upgrades it from a
prediction to a measurement: ≥122 rows. The honest resolutions are unchanged —
either make kind statically recoverable, or accept that
`--dump-native-registry` is the only admissible evidence for a kind claim.

---

## 10. Verified vs assumed

**Verified.**

* Every row in §3.2 and §6 — read out of two `--dump-native-registry` JSON files
  produced by `9964ca733`, in the two modes.
* That `register_p62_navigable_expansion` registered exactly 24 triples on
  exactly 4 classes with no loop and no `format!` — parsed from the source before
  deletion, and the same 24 appear in the dump.
* That `natural_compare_values` and every `p62_*` symbol had no referent outside
  `phases_late/collections.rs` — `grep` across all seven crates, excluding
  `target/` and `scratch/`.
* That both edited files are LF-only and `rustfmt --edition 2021 --check` clean.
  `collections.rs` carried **two pre-existing** rustfmt hunks before this lane
  (verified against `git show HEAD:`) and carries the same two after, at shifted
  line numbers. No new hunks.
* §7.1's simulation, and §4.1's mutation, both run.
* §8's six vectors, both modes, diffed against HotSpot 25.

**Assumed / not verified.**

* **That `registrar_drift.rs` compiles.** Still never given to `cargo`. New risk
  points beyond `G3-1` §9's list, in the order to check them: the match-ergonomic
  destructuring in `retake` (`for (t, (so, _)) in &a.drift`, copied from the
  pattern the previous version already used in a closure); `baseline_by_pass`'s
  `BTreeMap<&'static str, BTreeSet<Triple>>` and its `entry(pass).or_default()`;
  and the `filter(|(reason, _)| reason.as_str() != "arity<4")` in the M12 block,
  where `reason` binds as `&&String`.
* **That the Rust resolver produces the 1,244/1,380 in the table.** §2 is the
  case for believing it; §7 is what to do if it does not.
* **That deleting the `p62` arms changes nothing in a `--features synthetic-jdk`
  build.** It follows from the dumps and from `register()` being
  last-write-wins, but no synthetic build was made.
* That the six vectors would still pass with the change compiled in. They
  exercise the surviving bodies, which the change does not touch.
* The gate's runtime cost. Unchanged from `G3-1`: predicted seconds, never timed.

---

## 11. What this lane did NOT do

1. **It did not build or run `cargo`.** Third lane running.
2. **It did not decide the 935 closure-vs-closure rows** (N5), though §6 shows
   the instrument that can.
3. **It did not chase the 140 dump-absent triples** (N6) — a number it created.
4. **It did not model `NativeKind` in the gate.** It measured the consequence
   instead and pinned three examples.
5. **It did not touch `serialization.rs`, `native-io/src/lib.rs`,
   `native-collections/src/lib.rs`, `phases_late.rs`, `INDEX.md` or
   `README.md`** — nominations only.
6. **It did not check registration ORDER within a mode.** Unchanged from `G3-1`
   §10.6; the dump's `overwrote` column is the instrument and was `null` on every
   row this lane read.
