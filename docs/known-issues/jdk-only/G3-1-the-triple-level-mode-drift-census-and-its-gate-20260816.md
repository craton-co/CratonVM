# G3-1 — the triple-level mode-drift census, why it is 1,540 and not 2,412, which twins actually differ, and the gate that ratchets them

Status: **census complete at `(class, name, descriptor)` granularity across five
crates; gate written (`native-builtins/tests/registrar_drift.rs`); four twins
read and confirmed LIVE; F34-1's own worked example confirmed already FIXED.**
Wave G, lane G3, 2026-08-16. Picks up item 4 of `HANDOFF-20260814.md` §6 and
items 1–3 of `F34-1` §8.

---

## 0. NOTHING HERE WAS MEASURED ON A BINARY. READ THIS FIRST.

**This lane could not build, could not run `cargo`, and could not run
CratonVM.** Every number below was computed from Rust source text by a Python
scanner, and every behavioural claim about two implementations differing was
reached by *reading both bodies*, not by running either.

`HANDOFF-20260814.md` §2 is the standing rule and it applies to this file in
full: a prediction is not a result. Concretely —

* **The 1,540 is a source count, not a registry dump.** `--dump-native-registry`
  is the only thing that can say which body owns a slot. Nothing here has been
  compared against it.
* **The gate has never been compiled.** `rustfmt --edition 2021 --check` passes;
  that proves it parses, not that it type-checks. §9 says exactly what to do on
  its first run.
* **The baseline inside the gate came from a Python mirror of the gate's own
  algorithm, not from the gate.** F34-1 §9 had to make the same admission. §8
  gives the command that regenerates it.

Provenance tags used below: **MEASURED-ON-SOURCE** (a scanner counted it),
**SOURCE-VERIFIED** (a human read the lines and they say what is claimed),
**PREDICTED** (neither).

---

## 1. The headline

| | F34-1 (2026-08-13) | this lane (2026-08-16) |
|---|---:|---:|
| scope | `native-builtins` only | 7 crates¹ |
| drifting triples | **2,412** | **1,540** |
| …of which the two sides name the SAME free function | not computed | **446** |
| …of which at least one side is an inline closure (undecidable by reading names) | not computed | **935** |
| …of which the two sides name DIFFERENT free functions | not computed | **159** |
| synthetic-only passes | 284 | 280 |
| direct synthetic-only children of `register_synthetic_overrides` | 73 | **73, the identical set** |

¹ `native-builtins`, `native-collections`, `native-io`, `native-awt`, `vm`,
`native-builtins-crypto`, `native-builtins-security`.

The 73-name agreement is the cross-check that matters. Two independent scanners,
three days apart, different languages, different shipping-root definitions, and
the direct-family set is the same 73 names — so the disagreement about 2,412 vs
1,540 is **not** a disagreement about reachability. §2 says what it is.

---

## 2. Why 1,540 and not 2,412 — MEASURED-ON-SOURCE

Three causes, all reproduced. The first is the big one and it is a **defect in
F34-1's number**, not a difference of scope.

### 2.1 F34-1's per-family drift counts include passes that ship

F34-1 §4.3's `drift` column is per family, over the family's transitive subtree.
Reproducing that definition — attribute a triple to family `F` if **any** pass in
`closure(F)` registers it, then call it drift if any shipping-reachable pass
registers it — reproduces F34-1's table almost exactly:

| family | F34-1 | this lane, F34-1's definition | this lane, strict |
|---|---:|---:|---:|
| `register_phase50_natives` | 44/129 | **44/129** | 1/86 |
| `register_phase61_natives` | 45/121 | **45/121** | 40/116 |
| `register_phase63_natives` | 47/68 | 57/**68** | 38/49 |
| `register_phase64_natives` | 26/82 | 32/**82** | 6/56 |
| `register_phase58_natives` | 25/170 | 43/**170** | 20/147 |
| `register_http2_natives` | 38/90 | **38/90** | 38/90 |
| `register_atomic_boolean_natives` | 8/8 | **8/8** | 8/8 |

46 of the 73 rows land within 10% under that definition; 4 of 73 do under the
strict one. So F34-1's number is a per-family subtree count, and a synthetic-only
family's subtree contains **shipping-reachable helpers**. A triple registered by
such a helper is registered by the *same body* in both modes. It is not mode
drift; it is at worst an ordering question inside one mode.

Under F34-1's own definition this lane measures **3,156 distinct** triples (sum
over families 3,514, against F34-1's sum of 2,453) — higher than 2,412 because
this lane resolves more descriptors and sees more crates. Under the strict
definition it is **1,540**.

**The strict definition is the correct one for the question "do the two modes run
different code".** That is what this file counts and what the gate ratchets.

### 2.2 Resolution moves the number in both directions — MEASURED-ON-SOURCE

This lane resolves 13,167 of 14,063 `.register(` sites (93.6%) against F34-1's
unstated fraction, because it expands three things F34-1 did not:

* **`for x in ["a", "b"] { r.register(x, …) }`** — 818 sites expand to more than
  one triple. F34-1 warned that "four rows come out of a `for` loop". In this
  tree it is 818 sites, not four; a scan that cannot expand them loses whole
  classes (`java/util/NavigableMap`, `java/awt/Graphics2D`, every
  `AtomicIntegerFieldUpdater` row) with no other symptom.
* **`let cls = "java/io/ByteArrayOutputStream";`** followed by 12 uses of `cls`.
* **Registrars parameterised by class**
  (`fn register_math_natives(registry, class: &str)`) — 68 call sites, 55
  resolvable bindings. Their triples are attributed to the **calling** pass, not
  the callee, because a callee reachable from both modes can still be called
  with one class only from the synthetic arm.

### 2.3 Five crates cut both ways — MEASURED-ON-SOURCE

**320 of the 1,540 drifting triples have a registration site outside
`native-builtins`** (226 in `native-collections`, 94 in `native-io`). F34-1's
one-crate scope could not see those *shipping twins* at all; where it saw one it
was inferring. Conversely, seeing the other crates' call sites promotes four
passes out of `synthetic_only` (284 → 280), and each promotion removes its
triples from the drift set entirely.

---

## 3. What this census cannot see — MEASURED-ON-SOURCE, and load-bearing

`W7-5` §0 records a **60% over-statement** that came from trusting a
literal-only comparison. This section exists so nobody has to rediscover the
same lesson. The unresolved sites are counted, not waved away.

| blind spot | sites | effect on the number |
|---|---:|---|
| `.register(` on something that is not a `NativeMethodRegistry` (`thread_registry`, `SubstitutionRegistry`, `self` in `tck.rs`) | 499 | **none** — all 499 were classified by receiver and none is a native registration |
| descriptor or class built with `format!` | 51 | under-count |
| `for (name, desc) in [(..), (..)]` tuple loops | 155 | under-count |
| identifier this scanner cannot bind | 137 | under-count |
| call site outside any parsed `fn`, or inside a fn with no registry in scope | 29 | under-count |
| a conditional/expression argument | 11 | under-count |

Total unresolved that could hide drift: **383 of 14,063 sites (2.7%)**. The true
number is therefore **≥ 1,540**, never lower.

### 3.1 The blind spot that is not a counting problem: `NativeKind`

**A drifting row is a claim that two registrations exist. It is NOT a claim about
which body a `--jdk-only` process dispatches.** SOURCE-VERIFIED, from
`native-api/src/registry.rs`:

* `register_inner` refuses outright — never inserting — any registration whose
  effective kind is not `allowed_in(JdkOnly)`;
* `allowed_in` is `!matches!(self, NativeKind::SyntheticStub)`.

So a shipping twin tagged `SyntheticStub` does not win in `--jdk-only`; it is not
there at all, and the JDK's own bytecode serves the call. Kind is **ambient
registry state** (`set_category` / `with_category` / `register_with_kind`)
threaded through call chains, and no source scan can resolve it.

Two rows in this census are exactly that shape and are worth stating
individually:

* `java/util/concurrent/atomic/AtomicBoolean` — all 8 triples drift, and the two
  registrations **name the identical callbacks** (`native_ab_init_default`,
  `native_ab_get`, …). What differs is the kind: `lib.rs:8517` registers them
  inside `with_category(SyntheticStub)`, `util_concurrent_ext.rs:7989` sets
  `Bridge`. In `--jdk-only` all 8 are refused and real `AtomicBoolean` bytecode
  runs; in synthetic mode the `Bridge` copy is live. Same body, different
  admission.
* `java/time/Instant` — 16 triples, different bodies, and the SHIPPING twin
  (`register_synthetic_instant_stub_natives`, `lib.rs:41174`) sets
  `SyntheticStub`. So in `--jdk-only` **neither** copy is registered.

This is the single largest reason to treat every number here as a prediction
until `--dump-native-registry` has been read.

---

## 4. Risk ranking — which twins actually differ

A drifting triple only matters if the two bodies behave differently. Three tiers,
the first two mechanical, the third by reading.

### 4.1 Mechanical split — MEASURED-ON-SOURCE

| class | count | what it means |
|---|---:|---|
| both sides name the SAME free function | 446 | body-identical. Only kind (§3.1) and registration order can differ. **Benign for behaviour, not necessarily for `--jdk-only` admission.** |
| both sides name DIFFERENT free functions | 159 | candidate LIVE drift — two named implementations, mechanically identified |
| at least one side is an inline `\|ctx, args\|` closure | 935 | **undecidable by this method.** Not benign — unexamined. |

The 935 are the honest bad news: the commonest registration style in this tree is
an inline closure, and a textual method cannot compare two closures. They need
`--dump-native-registry` plus a differential, one family at a time.

### 4.2 The 159, by class — MEASURED-ON-SOURCE

`java/time/Instant` 16, `java/security/MessageDigest` 10,
`java/util/NavigableMap` 8, `java/util/TreeMap` 8,
`java/nio/channels/FileChannel` 6, `java/util/Collections` 6,
`java/util/concurrent/CompletableFuture` 5, `java/util/stream/IntStream` 5,
`java/lang/{Byte,Double,Float,Integer,Long,Short}` 4 each,
`java/lang/ClassLoader` 4, `java/lang/invoke/MethodHandles$Lookup` 4,
`java/util/NavigableSet` 4, `java/util/TreeSet` 4, `org/slf4j/Logger` 4,
then a tail of 1–3.

Highest-drift-ratio synthetic-only passes (drift/total registered):
`register_p62_navigable_expansion` 24/24, `register_p63_scheduled_executor`
18/18, `register_phase56_collectors_extras` 16/16,
`register_byte_array_output_stream` 12/12, `register_p63_resource_bundle` 9/9,
`register_atomic_boolean_natives` 8/8, `register_p59_management` 39/40,
`register_m18_concurrent_fixes` 35/41, `register_classloader_natives` 81/107,
`register_phase54_atomics` 79/113.

### 4.3 LIVE — read, and they differ

**LIVE 1 — `TreeMap` / `TreeSet` / `NavigableMap` / `NavigableSet` navigation, 24
triples.** SOURCE-VERIFIED.

`register_p62_navigable_expansion`
(`native-builtins/src/phases_late/collections.rs:374`, synthetic-only) binds all
eight `floor/ceiling/higher/lower` × `Key/Entry` methods on `TreeMap` and
`NavigableMap`, and four on `TreeSet`/`NavigableSet`, to `p62_tm_*` / `p62_ts_*`.
`p62_tm_ceiling_entry` (`collections.rs:676`) is a **linear scan over slots 0/1
using `natural_compare_values`**.

`register_tree_map_natives` (`native-collections/src/lib.rs`, shipping) binds the
same triples to `native_tm_*`. `native_tm_ceiling_entry`
(`native-collections/src/lib.rs:45650`) calls `tm_sync_native_state` first, has a
`tm_fast_with` BTree range path, honours a user `Comparator`, and deliberately
re-reads `data` after a comparator call because that call can move the heap
("Family-1 stale-`ObjectRef` fix", in the source).

They give different answers for any `TreeMap` with a custom `Comparator`, and the
`p62_*` copy has a live stale-reference hazard the other one was patched for.
**The copy every `--features synthetic-jdk` test measures is the weaker one.**

**LIVE 2 — `java/io/ByteArrayOutputStream`, 12 of 12 triples.**
SOURCE-VERIFIED. `register_byte_array_output_stream`
(`native-builtins/src/serialization.rs:5100`, synthetic-only) registers
`close()` and `flush()` as **no-ops**, with a correct-looking comment citing JDK
25's empty `close()`. `register_io_natives` (`native-io/src/lib.rs:6870`,
shipping) binds `close()` to `native_baos_close`, which dispatches
`BaosEvent::Close` and then `process_pipe_output_close`. The layouts agree
(slot 0 = `buf`, slot 1 = `count`, default capacity 32), so this is not a layout
bug — but in synthetic mode the process-pipe close path **does not run**, and
that is the mode the tests use. A second, smaller divergence in the same pair:
`new ByteArrayOutputStream(0)` allocates capacity 1 in the synthetic copy
(`.max(1)`) and 32 in the shipping one; neither throws, where HotSpot throws
`IllegalArgumentException` for a negative size — a shared defect, not drift.

**LIVE 3 — `java/time/Instant`, 16 triples.** SOURCE-VERIFIED, and see §3.1: the
shipping twin is `SyntheticStub`, so `--jdk-only` runs neither. Kept as the
standing counter-example to "the shipping copy wins".

**LIVE 4 — `org/slf4j/Logger`, 4 triples.** MEASURED-ON-SOURCE:
`slf4j_log_msg` (synthetic-only `register_slf4j_natives`) against
`slf4j_debug_msg` (shipping `register_slf4j_binder_stubs_pub`). The names alone
say the level is decided differently. Not read line by line; listed so the next
lane does not have to find it.

**BENIGN-BY-IDENTITY, worth naming — `AtomicBoolean`, 8 of 8.** The callbacks are
the same eight symbols on both sides. §3.1 is why it is still not nothing.

### 4.4 F34-1's worked example is STALE — SOURCE-VERIFIED

F34-1 §4.2 cites `register_pe_panama` / `structLayout` as the live instance,
38 of 52 triples shared with `register_p67_foreign_memory`.

**In the current tree, `MemoryLayout.structLayout` has exactly one registrant:
`phases_late/foreign_ffm.rs::register_p67_foreign_memory`.** F16 deleted the
whole group-layout family from `panama.rs::register_pe2_struct_layouts`; the
comment left in its place (`native-builtins/src/panama.rs:5829`) sets out both
layouts and the measured HotSpot answers. `register_pe_panama`'s subtree now
drifts on 17 triples, not 38.

That is a fix landing between F34-1 and this lane, not a disagreement — but it
means the one instance the campaign points at as proof is no longer reproducible,
and this file's gate pins the *fix* as its negative control so the twin cannot
come back unnoticed.

---

## 5. The gate — `native-builtins/tests/registrar_drift.rs`

A source witness reading the working tree through `env!("CARGO_MANIFEST_DIR")`
and its parent, **not** `include_str!`, for the reason
`registrar_reachability.rs` gives: `include_str!` bakes a compile-time snapshot,
which is the frozen model the gate exists to prevent. No VM boot; a
`#[cfg(feature = …)]` test can only guard the configuration it is compiled into,
and the whole defect is that one configuration is invisible from the other.

Four tests.

1. **`the_drift_scanner_is_not_vacuous`** — twelve measured floors, a
   brace-balance self-check, a two-sided control, and three resolver witnesses.
2. **`the_baseline_is_well_formed`** — no duplicate rows, no zero rows, the
   per-pass sum cannot fall below the distinct total, the floor must sit below
   the ceiling, and no row may name a pass that is no longer synthetic-only
   (a stale exemption is a hole).
3. **`no_new_mode_drift`** — the ratchet. Per-pass ceiling from
   `DRIFT_BASELINE`; a pass absent from the table has a ceiling of **zero**.
   Plus a global ceiling, so drift moving between passes cannot net out.
4. **`the_known_live_twins_still_drift`** — the four §4.3 rows pinned
   individually, because a count ratchet cannot tell "these bodies disagree"
   from "these names point at one function".

### 5.1 The two-sided control

A one-sided control is worthless here in both directions, so both are pinned:

* **POSITIVE** — `java/util/TreeMap.ceilingEntry` MUST drift (§4.3 LIVE 1). A
  scanner that has stopped resolving descriptors reports no drift for everything
  and would pass a negative-only control.
* **NEGATIVE** — `java/lang/foreign/MemoryLayout.structLayout` MUST NOT drift
  (§4.4). A scanner that has stopped discriminating reports drift for everything
  and would pass a positive-only control.
* **The negative control is itself guarded against vacuity**: a third assertion
  requires `structLayout` to be present in the census *at all*. Without it the
  negative control passes when the resolver simply loses the triple — which is
  the same "confident, vacuous zero" F34-1 §2.1 recorded twice.

### 5.2 Mutation analysis — what it catches, what it does not

Applied **in the head, against the written algorithm**, because `cargo` was
forbidden. F34-1 §7 ran its mutants through a Python mirror; this lane's mirror
computes the census but does not re-implement the Rust assertions, so the
right-hand column below is **PREDICTED**, not observed. That is a real weakness
and it is stated rather than dressed up.

| # | mutation | which assertion should fire |
|---|---|---|
| M1 | a new triple registered by both a synthetic-only and a shipping pass | `no_new_mode_drift` — its pass exceeds its ceiling, or has a ceiling of 0 |
| M2 | a synthetic-only pass gains a shipping call site (promotion) | `the_baseline_is_well_formed` — stale row; and `MIN_SYNTHETIC_ONLY` if many |
| M3 | `NativeMethodRegistry` renamed | `MIN_PASSES`, then everything below it |
| M4 | `register_synthetic_overrides` ceases to be locatable | **`MIN_SYNTHETIC_OVERRIDES_BODY` only.** This is F34-1 §M4b reproduced deliberately: its `families` and `closure` checks stayed green and a body-size floor was the sole survivor |
| M5 | the descriptor resolver degrades (e.g. `let` binding support dropped) | `MIN_RESOLVED_SITES`, `MIN_TRIPLES`, `MIN_TOTAL_DRIFT`, and the three `RESOLVER_WITNESSES` |
| M6 | loop expansion removed | `MIN_LOOP_EXPANDED_SITES` (and `MIN_TRIPLES`) |
| M7 | the `panama.rs` `structLayout` twin restored | NEGATIVE control |
| M8 | the `p62_*` navigable family deleted or collapsed | POSITIVE control **and** `the_known_live_twins_still_drift` |
| M9 | a literal-blanking bug that eats code (this lane hit one) | the brace-balance self-check |
| M10 | `register_builtins` taken as a shipping root | `MIN_SYNTHETIC_ONLY`, then `MIN_DIRECT_SYNTHETIC_ONLY` |
| M11 | **one drifting triple swapped for another inside one pass** | **NOTHING.** The ceiling is a count, not a set |
| M12 | **new drift arriving through a `format!` descriptor, a tuple `for` loop, or a class-parameterised registrar** | **NOTHING.** §3's blind spots are blind to the gate too |
| M13 | **two registrations of one triple with the same body and different `NativeKind`** | **NOTHING.** §3.1 |

M11–M13 are the honest holes. M11 is a deliberate trade: pinning all 1,540
triples exactly would catch it and would also redden the gate on any harmless
resolver improvement — and a gate that reddens for no reason gets relaxed, which
is how `W6-5-vacuous-tests.md` starts.

### 5.3 Why the baseline is a ceiling and not an equality

`registrar_reachability.rs` ratchets in both directions on purpose: a name
leaving its allow-list is good news and still fails, because a stale exemption is
a hole. This gate does **not**, and the reason is §0: it has never been compiled,
so the exact number its Rust resolver will produce is not known. An equality
would then be a coin flip on the first run. A ceiling fails on every increase —
which is the direction that matters — and reports a decrease in its printed line
without reddening.

**This is weaker than F34-1's gate, on purpose, and it should be tightened to an
equality by whoever first runs it and sees the real number.** That is
nomination N7.

---

## 6. The gate's numbers, and the mirror that produced them

The Rust gate resolves a deliberately **narrower** set of forms than the census
in §1–§4, so that the two can be kept in step by hand: string literals,
`let x = "lit" | CONST` inside the enclosing `fn` (first binding wins),
file-scope and crate-scope `&str` consts (ambiguous names refused), and
single-variable `for x in [ … ]` over literal/const elements. Nothing else.

Under those rules — **MEASURED-ON-SOURCE by the mirror, `GATE_MODE=1`**:

| | |
|---|---:|
| `.rs` files | 361 |
| `fn` definitions | 33,710 |
| registration passes | 872 (838 distinct names) |
| `.register(` sites | 14,063 |
| …resolved | 12,600 |
| …expanded through a `for` loop | 695 |
| distinct triples | 11,422 |
| shipping-reachable passes | 521 |
| synthetic-only passes | 280 |
| direct synthetic-only children | 73 |
| `register_synthetic_overrides` body | 104,360 bytes |
| **drifting triples** | **1,266** |
| baseline rows (synthetic-only passes with drift) | 112 |

The gap between 1,266 (gate mode) and 1,540 (full census) is entirely the four
resolution features the gate deliberately does not implement. Both numbers are in
the file: 1,266 is the ceiling the gate enforces, 1,540 is what this record
claims about the tree.

---

## 7. NOMINATIONS

This lane could edit exactly one file (the new test). Everything below is a
change it identified and did not make. Ordered by severity.

**N1 — `p62_tm_*` / `p62_ts_*` must not shadow `native_tm_*` / `native_ts_*`.**
`native-builtins/src/phases_late/collections.rs:374`
(`register_p62_navigable_expansion`), all 24 registrations. The synthetic-only
bodies at `collections.rs:676` ff. are comparator-blind linear scans without the
stale-`ObjectRef` refresh that `native-collections/src/lib.rs:45650` ff. carry.
**Change:** delete the `TreeMap`/`TreeSet` arms of
`register_p62_navigable_expansion` and let `register_tree_map_natives` /
`register_tree_set_natives` serve both modes — but first check F34-1 §5's trap:
verify the `NavigableMap`/`NavigableSet` *interface* triples are also registered
by the shipping pass, because deleting a capability that only the synthetic arm
provides is the same error in the other direction. (This lane's data says
`NavigableMap.ceilingEntry` IS registered by `register_tree_map_natives`, so the
delete looks safe — PREDICTED, verify with `--dump-native-registry`.)

**N2 — `ByteArrayOutputStream.close()` / `flush()` disagree.**
`native-builtins/src/serialization.rs`, the last two registrations of
`register_byte_array_output_stream` (the `r.register(cls, "flush", …)` and
`r.register(cls, "close", …)` pair at the end of the function) register no-ops,
while `native-io/src/lib.rs:6897`–`6898` bind `native_baos_close` /
`native_baos_flush`. **Change:** drop those two registrations from
`serialization.rs` so `native-io`'s pipe-aware bodies serve both modes; they are
registered earlier and are not otherwise overwritten.

**N3 — `new ByteArrayOutputStream(negative)` must throw.** Both twins accept it:
`native-builtins/src/serialization.rs` clamps with `.max(1)`,
`native-io/src/lib.rs:3822` (`native_baos_init_capacity`) falls back to 32.
HotSpot throws `IllegalArgumentException: Negative initial size: …`. **Change:**
raise the exception in `native_baos_init_capacity` and delete the synthetic
twin per N2. Not drift — a defect both copies share, found while diffing them.

**N4 — `register_synthetic_instant_stub_natives` is on the shipping path and
named as if it were not.** `native-builtins/src/lib.rs:41174`, called from
`native-builtins/src/reflect_annotations.rs:758`. It sets
`NativeKind::SyntheticStub`, so `--jdk-only` refuses all 16 of its rows and the
name is accurate about intent — but a *shipping* registrar called
`register_synthetic_*` is precisely the kind of name that made F34-1 §0's
premises wrong. **Change:** rename to `register_instant_stub_natives_shipping`
or add a doc comment stating the arm it is on and that its kind is what removes
it in strict mode.

**N5 — `native-builtins/tests/registrar_reachability.rs` should adopt the
brace-balance self-check.** Its `blank()` shares this scanner's shape. This
lane's Python version of the same algorithm had a `'\\'` char-literal bug that
blanked real code — including braces — from a byte-literal backslash to the next
apostrophe in the file, which silently truncated a 770-line `fn` and dropped 277
registration sites. `registrar_reachability.rs`'s char-literal branch searches
forward for the next `'` within 4 bytes and is **correct**; the check is still
worth adding because it is two lines and it fails loudly on the whole family of
blanking bugs. **Change:** after `blank()`, assert `count('{') == count('}')` per
file.

**N6 — `NativeKind` should be recoverable from source, or the registry dump
should be the only allowed evidence.** `native-api/src/registry.rs` `allowed_in`
means kind decides `--jdk-only` admission, and §3.1 shows a body-identical pair
(`AtomicBoolean`) whose two registrations differ only in kind. No source scan can
resolve ambient `set_category` state. **Change:** either add a
`#[track_caller]`-style debug assertion that records the effective kind per
registration into the census (it already does — `NativeCensusEntry.kind`), and
document `--dump-native-registry` as the *only* evidence for a kind claim; or
accept that every drift record in this directory carries §3.1's caveat.

**N7 — tighten this gate's ratchet to an equality once it has been run.**
`native-builtins/tests/registrar_drift.rs`, `no_new_mode_drift`. §5.3 explains
why it ships as a ceiling. **Change:** on the first green run, replace
`count > allowed` with `count != allowed` and re-take `DRIFT_BASELINE` and
`BASELINE_TOTAL_DRIFT` from the gate itself rather than from the mirror.

**N8 — the 935 closure-vs-closure drifting triples need a differential, not a
reader.** No single file. **Change:** run each of the top drift families under
`--dump-native-registry` in both modes and diff `owns_slot` + a behavioural probe;
`HANDOFF-20260814.md` §4's loop is the method. This is the largest remaining
unexamined surface after this lane, and it is un-gated for the same reason F34-1
left drift un-gated: the tooling to decide it is a running VM.

---

## 8. Reproducing the numbers

The scanner is Python and lives in the lane's scratchpad, not in the repo (it is
a measurement, not a build input). Its algorithm, in the order it runs:

1. Read every `.rs` under `<crate>/src` for the seven crates, skipping
   `tests`/`benches`/`examples`/`fuzz`/`target`.
2. Produce two byte-parallel buffers per file: comments blanked; and comments
   plus string/char literal **content** blanked. Assert per file that
   `count('{') == count('}')` in the second — this is N5, and it caught this
   lane's own blanking bug.
3. Parse every `fn`, with body spans by brace matching and parent links by a
   nesting stack. Mark `testish` (`#[test]`, `#[cfg(test)]`, or inside a
   `#[cfg(test)] mod`) and `syn_gated` (`#[cfg(feature = "synthetic-jdk")]`).
4. Build the call graph over identifiers in **call position and not preceded by
   `.`**. This matters: a bare-identifier scan turns every `r.register(…)` into
   an edge to the several `fn register(r: &mut NativeMethodRegistry)` in
   `native-builtins/src/jca/*`, which inflates the synthetic closure by ~70
   names and moves whole families across the shipping line. This lane made that
   mistake and measured it: `synth_reach` was 533 before the fix and 470 after.
5. Shipping roots = every pass referenced from a **non-pass** `fn` or at module
   level, minus `register_synthetic_overrides`, minus every
   `#[cfg(feature = "synthetic-jdk")]` pass. The last exclusion is F34-1 §2.1's
   trap: `register_builtins` is itself gated, is `pub`, and is referenced from
   `vm/src/native/builtins.rs`.
6. `synthetic_only = reach(register_synthetic_overrides) − reach(shipping roots)`.
7. Extract every `.register(` with ≥ 4 arguments; resolve arguments 1–3 through
   loop environments, `let` bindings, file consts, crate consts; count every
   failure by reason.
8. `drift = { t : ∃ p ∈ registrants(t) ∩ synthetic_only ∧ ∃ q ∈ registrants(t) ∩ shipping }`.

`GATE_MODE=1` restricts step 7 to exactly the forms the Rust gate implements
(§6) and writes `gate.json`; the `DRIFT_BASELINE` table is its per-pass drift
counts. Regenerating it is `GATE_MODE=1 python census.py` followed by the
baseline printer — both scripts are in the lane scratchpad and should be
re-created from this section rather than trusted, since scratchpads do not
survive.

The three fragments that decide the answer, verbatim, so nobody has to guess
what step 4 and step 8 meant:

```python
# step 4 — the `.`-exclusion. Without it, `synth_reach` is 533; with it, 470.
REF = re.compile(r"(?<![A-Za-z0-9_.])[A-Za-z_][A-Za-z0-9_]*")

def refs(text):
    """Identifiers in call/reference position. NOT preceded by `.`:
    `r.register(..)` is a METHOD call on the registry, and counting it as a
    reference to native-builtins/src/jca/*::register (real passes of that
    name) wires every registrar in the tree to every jca pass."""
    return set(REF.findall(text))

# step 5/6 — roots and closures
syn_gated_names = {f.name for _, f in passes if f.syn_gated}
synth_reach = closure([SYNTH_ROOT])
ship_roots  = {n for n in nonpass_refs
               if n != SYNTH_ROOT and n not in syn_gated_names}
ship_reach  = closure(ship_roots)
synth_only  = synth_reach - ship_reach

# step 8 — the STRICT drift definition. `triples` maps a
# (class, name, descriptor) to the set of passes that register it.
drift = {}
for tri, regs in triples.items():
    names = {r[0] for r in regs}
    so = names & synth_only
    sh = names & ship_reach
    if so and sh:                      # BOTH sides, or it is not mode drift
        drift[tri] = {"synth_only": sorted(so), "shipping": sorted(sh)}
```

F34-1's number is what you get by replacing the last block with "attribute a
triple to family `F` if any pass in `closure(F)` registers it, then call it
drift if any shipping-reachable pass registers it" — which counts a shipping
helper's own triples as drift against the family that happens to call it. §2.1.

---

## 9. Verified vs assumed

**Verified.**

* The 73 direct synthetic-only families are the identical set to
  `registrar_reachability.rs`'s `DELIBERATE_SYNTHETIC_ONLY_FAMILIES` — compared
  name by name.
* §2.1's reproduction of F34-1's per-family definition — computed, and 46 of 73
  rows land within 10% of F34-1's published numbers against 4 of 73 under the
  strict definition.
* Every §4.3 body diff — both implementations read, with file and line.
* §4.4's claim that `structLayout` has one registrant — both the census and a
  direct `grep` of `structLayout` across `native-builtins/src` and `vm/src`.
* §3.1's `allowed_in` / `register_inner` behaviour — read at
  `native-api/src/registry.rs:4886` and `:6096`.
* The 499 `arity<4` sites are all non-registry `register` methods — classified by
  receiver, every distinct receiver listed.
* The gate parses and is `rustfmt --edition 2021 --check`-clean, with zero CR
  bytes.
* The gate's four pinned `MUST_DRIFT` rows, three `RESOLVER_WITNESSES` and both
  controls all hold **against the mirror's `gate.json`**.

**Assumed / not verified.**

* **That `registrar_drift.rs` compiles.** It has never been given to `cargo`. A
  type error, a borrow error or a `std` API misremembered would appear only on
  `cargo test -p cratonvm-native-builtins --test registrar_drift`. Known risk
  points, in the order this lane would check them: the nesting-stack sweep in
  section 6 of the file (`stack.last().copied()` is used deliberately so no
  borrow crosses a `pop()`); `&t[back - 2..back] == b"fn"`, which relies on the
  blanket `PartialEq<&[B; N]> for &[A]` impl and is copied verbatim from
  `registrar_reachability.rs`; and the `let_cache` mutable borrow living across
  the site loop body.
* **That the gate's numbers match the mirror's.** They are two implementations
  of one spec written by one lane in one sitting, which is the weakest form of
  agreement there is. If the first run is red on `no_new_mode_drift`, re-take
  the baseline from the gate's own output before assuming a real regression; if
  it is red on a FLOOR or a CONTROL, that is a genuine scanner difference and
  must be chased.
* Its runtime cost. It reads roughly 80 MB of source once per test process
  (`OnceLock`-shared across four tests) and does one `O(n log n)` sweep per file.
  Seconds, predicted, not measured.
* **Every behavioural claim in §4.3.** Two bodies that read differently usually
  behave differently; usually is not always. None of the four was run.
* That `--jdk-only` actually dispatches the shipping body for any of the 1,540.
  §3.1 is the reason that is not a safe default.

---

## 10. What this lane did NOT do

1. **It did not run anything.** No build, no `cargo test`, no VM, no
   `--dump-native-registry`. Everything is source.
2. **It did not decide the 935 closure-vs-closure rows.** That is 61% of the
   drift and it needs a running VM. See N8.
3. **It did not resolve `format!` descriptors, tuple `for` loops, or
   class-parameterised registrars *in the gate*.** The census resolves the last
   of those; the gate does not, and 383 sites remain unresolved in both.
4. **It did not model `NativeKind`.** §3.1. This is the difference between "two
   registrations exist" and "two bodies run", and this lane can only claim the
   first.
5. **It did not touch any existing source file.** Seven nominations, zero edits
   outside `native-builtins/tests/registrar_drift.rs` and this record.
6. **It did not check registration ORDER within a mode.** Which of several
   shipping registrants wins in `--jdk-only` is decided by call order inside
   `register_essential_natives_with_shims`, which this scan does not linearise.
   For the 446 same-callback rows that is irrelevant; for the rest it is a second
   question the gate does not ask.
7. **It did not re-audit F34-1's class-granularity verdicts.** Its "no capability
   gap at class granularity" conclusion is untouched; this file only shows that
   its drift *number* counts shipping helpers.
