# G37-1 — marking the bypasses, and the third family `G33-1` did not probe

**Status:** MEASURED (the census numbers below, and the correction in §2 —
taken on this host against a binary that does **not** contain this lane's
edits), SOURCE-VERIFIED (the sweep), **PREDICTED (the code changes — this lane
could not build, run or test its own edits).** **Provenance:** every number was
taken against `C:/craton/target-rel2/release/cratonvm.exe`, release, stated to
be built from `9964ca733`. `C:/craton/target-rel3/` does not exist on this host.
The oracle is not involved — this record is about CratonVM's own instrument.

| | |
|---|---|
| subject | `--dump-native-registry`'s `invocations` column, and the paths that lose calls from it |
| predecessor | `G33-1` diagnosed the defect and built the API; **nothing set the bit** |
| this lane's files | `vm/src/jit/helpers.rs`, `vm/src/runtime/interpreter/invoke.rs` |
| probe | `scratchpad/g37/ReflProbe.java` |
| regression | `RJdkHello RCollections RStrings RJdkCollections RMapGcStress RSyncMethodJit RJitGc RFieldSiteCache` — **8 passed, 0 failed** |
| tree | `claude/jdk-only-mode-completion-1351c0` |

---

## 0. The headline

| finding | |
|---|---|
| the bit is now set on **22 triples**, across two bypass families in this lane's two files | PREDICTED |
| **`G33-1` §0's headline — "the counter is exact in `--nojit` with intrinsics off" — is FALSE** | MEASURED, §2 |
| the counter-example is a **third bypass family**, in `invoke.rs`, arm-independent AND intrinsic-independent | MEASURED + SOURCE-VERIFIED |
| **`G20-1` §8's original claim was right; `G33-1` §3's correction of it was wrong** | MEASURED, §2 |
| `HashMap.put`/`get` read **0**, not 2,000, in the JIT arm of this binary | MEASURED, §1 |
| the JIT direct-helper family is **seven** helpers, not the two `G33-1` §8 N1 named | SOURCE-VERIFIED, §3 |
| `G33-1` §2's "the direct-call family is not uniformly broken" (from `StringLatin1.toLowerCase`) does not hold as reasoning | SOURCE-VERIFIED, §3 |
| the interpreter's intrinsic table (`G33-1` N2) is **NOT in this lane's files** and is still unmarked | §6 N1 |

**The single most useful sentence in this record:** there is **no**
configuration in which this instrument is exact, and `--nojit` +
`CRATONVM_DISABLE_INTRINSICS=1` — the recipe `G33-1` §4 tells every reader to
take censuses in — still loses one call per reflective call site, measured, at
every workload size.

---

## 1. What the instrument actually reads today — MEASURED

`scratchpad/g37/ReflProbe.java`, one process per row: *n* reflective
`Method.invoke`, *n*/10 `Constructor.newInstance`, *n* `HashMap.put`, *n*
`HashMap.get`. All numbers are the `owns_slot: true` row.

*n* = 100,000:

| native | expected | JIT | `--nojit` | `--nojit` + intrinsics OFF |
|---|---:|---:|---:|---:|
| `Method.invoke` | 100,000 | **1,999** | **99,999** | **99,999** |
| `Constructor.newInstance` | 10,000 | 10,000 | **9,999** | **9,999** |
| `HashMap.put` | 100,000 | **0** | 100,000 | 100,000 |
| `HashMap.get` | 100,000 | **0** | 100,000 | 100,000 |
| `Integer.valueOf(I)` | ≫ 0 | 2 | 2 | 400,013 |
| `Integer.intValue()` | ≫ 0 | 1 | 1 | 200,000 |
| `Thread.currentThread()` | > 0 | 0 | 0 | — |

Three separate things are visible here and they are worth keeping apart.

**`HashMap` reads ZERO in the JIT arm on this binary.** `G33-1` §2 measured
2,000 on `target-rel` (`783685c34`); the same probe on `9964ca733` loses *every
single call*. This is the exact failure mode the brief names — "one lane
already concluded a body was dead from a zero" — reproduced live, on the
hottest collection native in the VM, with the `--nojit` arm proving 100,000 real
calls in the same program.

**`Integer.valueOf`/`intValue` are the autobox latch**, unchanged from `G33-1`
§2: 2 and 1 in both arms, 400,013 and 200,000 once
`CRATONVM_DISABLE_INTRINSICS=1` is set. Confirmed, not new.

**The reflection rows are new, and they are the important ones.** See §2.

## 2. `G33-1` §0's headline is false, and `G20-1` §8 was right all along — MEASURED

`Method.invoke` and `Constructor.newInstance` are short by **exactly one call**,
in **every** configuration, including the one `G33-1` §4 prescribes as exact:

| *n* (`--nojit`) | `Method.invoke` expected / got | `Constructor.newInstance` expected / got | `HashMap.put` expected / got |
|---:|---|---|---|
| 1,000 | 1,000 / **999** | 100 / **99** | 1,000 / 1,000 |
| 10,000 | 10,000 / **9,999** | 1,000 / **999** | 10,000 / 10,000 |
| 100,000 | 100,000 / **99,999** | 10,000 / **9,999** | 100,000 / 100,000 |
| 200,000 | 200,000 / **199,999** | 20,000 / **19,999** | 200,000 / 200,000 |

The deficit is **constant at 1** and does not scale, while a control in the same
process (`HashMap.put`) is exact. `CRATONVM_DISABLE_INTRINSICS=1` does not close
it. That is the signature of *the first call at a site taking one route and every
later call taking another*, and the source says which: `try_stackless_invoke`
serves the first reflective call through its `NCS_METHOD_INVOKE` /
`NCS_CONSTRUCTOR_NEW_INSTANCE` arms, which hold a `NativeCallback` and **no
`NativeMethodId`**, and return `Handled` before ever reaching the function's
`record_invocation`. Subsequent calls take the counted `resolve_step1_native`
path.

Three consequences, and none of them is small:

1. **`G33-1` §0's "the counter is exact in `--nojit` with intrinsics off —
   MEASURED, every native probed" is false.** It was true of the natives that
   record probed. It is not a property of the configuration. `G33-1` §4's
   closing instruction — "For an exact `invocations` column, run with `--nojit`
   and `CRATONVM_DISABLE_INTRINSICS=1`" — should be read as *"for the least
   inexact column"*, and `G33-1` §10's reproduce block inherits the same caveat.
2. **`G20-1` §8's "it under-reports in the `--nojit` arm too, so it is not
   simply the JIT bypassing the counter" was CORRECT**, and `G33-1` §3's
   amendment #1 — which agreed with the conclusion while declaring the evidence
   wrong and the `--nojit` arm exact — needs its own amendment. There *is* an
   arm-independent, intrinsic-independent mechanism. It is the exotic-arm family
   in `invoke.rs`, and it is the one neither record probed.
3. **A native reached only through these arms reads `invocations: 0` forever.**
   A single-call-site native that is called once is indistinguishable from a
   dead one. `G33-1` §4's "zero proves nothing" was already the right reading;
   this is a third, independent reason it is right.

The magnitude is one call per call site, so no existing conclusion in this
directory that rests on a *large* count is disturbed. What is disturbed is the
claim that any configuration is exact.

## 3. The sweep — SOURCE-VERIFIED

The brief asked for any *other* path in this lane's two files that dispatches a
native without holding a `NativeMethodId`. Both files had one, and the JIT file's
was larger than `G33-1` §8 N1 described.

### `vm/src/jit/helpers.rs` — seven thin direct-call helpers, not two

`G33-1` §8 N1 nominated `jit_hashmap_put_direct` and `jit_hashmap_get_direct`.
The file has **seven** direct-call helpers and **none** of them counts:

| helper | triple(s) | counts? |
|---|---|---|
| `jit_integer_value_of_direct` | `Integer.valueOf(I)` | no — neither the TLAB arm nor the cold `safe_native_call` arm |
| `jit_integer_int_value_direct` | `Integer.intValue()` | no |
| `jit_hashmap_put_direct` | `HashMap.put` | no |
| `jit_hashmap_get_direct` | `HashMap.get` | no |
| `jit_string_latin1_to_lower_direct` | `StringLatin1.toLowerCase` | no |
| `jit_concurrent_hashmap_get_direct` | `ConcurrentMap.get` / `ConcurrentHashMap.get` | no |
| `jit_thread_current_thread_direct` | `Thread.currentThread()` | no |

Two of `G33-1` §2's supporting statements do not survive this reading.

* *"The `Integer` siblings in the same file route through
  `call_integer_native_raw`, whose doc is explicit that its open-coded arms must
  be counted."* True of the `jit_invoke_dispatch` route. The **standalone direct
  helpers** `jit_integer_value_of_direct` / `jit_integer_int_value_direct` are
  separate bodies that never enter that wrapper, and a site bound to one of them
  never takes the route that does. The inconsistency `G33-1` read as "the tell"
  is real, but the counted sibling is a different code path, not a different
  native.
* *"`StringLatin1.toLowerCase` … has a direct helper too, and it counted, so the
  direct-call family is not uniformly broken."* The body of
  `jit_string_latin1_to_lower_direct` contains **no census call at all**. That
  probe's exact figure cannot have come from the helper; it must have come from
  sites that never bound it. The family **is** uniformly broken.

Both `ConcurrentMap.get` and `ConcurrentHashMap.get` are marked: they are two
registry slots holding the same `native_chm_get`, the helper's fallback
`JitInvokeInfo` names the interface while its fast arm only fires for an exact
`ConcurrentHashMap` receiver, and the row that loses the call is whichever the
fallback would have resolved.

### `vm/src/runtime/interpreter/invoke.rs` — the third family

Eleven arms of `try_stackless_invoke` produce a callback with no id. Four of
them return `Handled` before the function's `record_invocation` is even in
scope; the other seven reach it with `step1_native_id == None`, which the code
already calls "the wave-2 census gap noted at step 1" — a gap acknowledged in a
comment and invisible to every reader of the dump.

Fourteen constant triples are now marked: the four `JarFile` constructor
descriptors, `ZipFile.close`, `Method.invoke`, `Constructor.newInstance`, the
four `DowncallHandle` arms, and the three `MethodHandle.invoke*` bridges. The
last three are the same species as `G33-1` §8 N4's `vm_exec.rs` finding —
signature-polymorphic dispatch that resolves the erased `Object[]` descriptor —
seen from the interpreter's stackless path instead.

Three arms are **not** marked because their triple is not constant and cannot be
enumerated at compile time: the superclass walk
(`find(&parent.name, method_name, descriptor)`), the three
`sun/security/ssl/*Impl` → `javax/net/ssl/*` aliases, and
`surefire_lazy_launcher_discover_native`. These remain silent floors. §6 N3.

## 4. What changed — PREDICTED (written, `rustfmt`-clean, not compiled and not run)

Two files, both owned by this lane. **The bit is set at bind time in both, never
per call.**

### `vm/src/jit/helpers.rs`

* **`DIRECT_CALL_HELPER_NATIVES`** — the eight triples of §3's seven helpers,
  each row carrying the helper that serves it.
* **`mark_direct_call_helper_natives_incomplete(&SharedVm)`**, called from
  `build_helpers_opt`'s direct-helper block, immediately after the seven
  `set_*_direct_fn` calls — the point the helper addresses are published into
  `cratonvm_jit`'s `*_DIRECT_FN` cells, and the last moment before a compiled
  site can be pointed at one. Latched on `(vm_identity, registry generation)`,
  the same pair `jit_invoke_dispatch`'s site cache uses, so a `RegisterNatives`
  that adds one of these triples later is picked up on the next compile.
* **`mark_direct_call_helper_natives_incomplete_in(&NativeMethodRegistry)`** —
  the registry half, split out so the marking is testable without a live VM.

### `vm/src/runtime/interpreter/invoke.rs`

* **`UNCOUNTED_STACKLESS_NATIVES`** — §3's fourteen constant triples, with the
  three non-enumerable arms named in the doc rather than silently omitted.
* **`mark_stackless_exotic_natives_incomplete(&SharedVm)`**, `#[cold]`, same
  latch. Called from five points: the four early-returning arms (`JarFile`
  `<init>`, `ZipFile.close`, `Method.invoke`, `Constructor.newInstance`) and the
  **`else` of the existing `if let Some(id) = step1_native_id`** — which is
  exactly the "a native is about to run and nothing will count it" branch, and
  turns a code comment into a fact the census carries.
* **`mark_stackless_exotic_natives_incomplete_in(&NativeMethodRegistry)`**, and
  a new `#[cfg(test)] mod tests` (this file had none).

### Why bind time and not `record_invocation`

`G33-1` §5 measured a hot-path counter at **+9.2 ns/call** against a 1.25 ns
baseline — ~6% of the ~141 ns native boundary, but plausibly *the entire margin*
a thin direct-call helper exists to buy, and paying it would partly undo
`perf/halfgap-20260717`. This lane could not build a binary, so it could not run
the interleaved A/B the brief requires before choosing option (a), and a
measurement it cannot take is not a measurement it may assume. Option (b) it is,
on both sides.

The `invoke.rs` side is a genuinely closer call and is left as a nomination
rather than a decision: those arms take the **full `safe_native_call` funnel**,
against which +9.2 ns is the same ~6% the counter already costs everywhere else
it sits. Making them *exact* rather than merely honest is cheap in principle —
it means turning `find(triple)` into `resolve_id(triple)` + `callback_of(id)`,
a transformation this same file already performs twice with the justification
"one hash either way" — but it touches eleven arms of the interpreter's hottest
function, and the brief is explicit that a regression there is worse than an
imprecise counter. §6 N4.

### What the bit claims

That a bypassing path is **wired** for the slot, not that a bypassing dispatch
has **happened**. That is the statically true statement and the only one a
report-time reader could act on. It also means the JIT marks appear only in runs
that actually compile something: under `--nojit`, `build_helpers*` is never
called, and those runs keep an unmarked — and, for the direct-helper slots,
genuinely exact — census.

### Tests

Five, in the existing `#[cfg(test)] mod tests` of `helpers.rs` and the new one
in `invoke.rs`:

* `every_thin_direct_call_helper_is_on_the_census_bypass_list` — every
  `*_DIRECT_INFO` static's triple is on the list, and the list is duplicate-free.
  A new helper with no list row would otherwise fail silently, in the one
  direction `mark_invocations_incomplete`'s doc says this instrument must never
  err.
* `marking_the_direct_call_helpers_turns_their_rows_into_floors` — the marked
  rows become floors, a `System.identityHashCode` control stays exact, the
  **tally survives marking**, the mark is idempotent, and the doubt reaches
  `census()`.
* `marking_an_empty_registry_marks_nothing_and_does_not_panic` (both files) —
  `register_collections_natives` is a separate registrar from
  `register_essential_natives`, and this code runs on the compile path and on the
  interpreter's hottest function.
* `the_signature_polymorphic_rows_use_the_erased_descriptor` — the
  `MethodHandle` / `DowncallHandle` rows must stay on
  `([Ljava/lang/Object;)Ljava/lang/Object;`. "Tidying" them to concrete
  descriptors would leave the table resolving nothing and the marks absent,
  which reads identically to a fixed instrument.
* `marking_the_stackless_exotic_arms_turns_their_rows_into_floors` — same shape,
  with the counted `resolve_step1_native` path as the control that must keep
  claiming exactness.

## 5. Which configurations are exact, and which are floors

Stated plainly, because this is the deliverable a reader will quote. "Exact"
below means *after this lane's marks land*; the marks change no number, only
what a number licenses.

| configuration | `HashMap.*`, `Integer.*`, `Thread.currentThread`, `StringLatin1.toLowerCase`, `ConcurrentMap.get` | the interpreter's intrinsic table (`Math.abs`, `Object.hashCode`, …) | `Method.invoke`, `Constructor.newInstance`, `MethodHandle.invoke*`, `DowncallHandle.*`, `JarFile`/`ZipFile` bridges | the SSL aliases, the superclass walk, `surefire_*` | everything else |
|---|---|---|---|---|---|
| JIT, intrinsics on | **floor, marked** | floor, **unmarked** | **floor, marked** | floor, **unmarked** | exact |
| `--nojit`, intrinsics on | exact (helpers unwired) | floor, **unmarked** | **floor, marked** | floor, **unmarked** | exact |
| `--nojit` + `CRATONVM_DISABLE_INTRINSICS=1` | exact | exact | **floor, marked** | floor, **unmarked** | exact |

* **Nothing is exact everywhere.** The third column is non-empty in every row,
  which is §2's finding.
* **The two unmarked columns are the honest residue of this lane**, and both are
  nominations: the intrinsic table because its install site is not in this
  lane's files (§6 N1), the dynamic arms because their triples are not constant
  (§6 N3).
* **`owns_slot` is unaffected** and remains the authoritative answer to "which
  body would run". `HANDOFF-20260814` §4's recommendation of this dump stands.
* **No number in any existing record changes.** What changes is that 22
  rows will now say so themselves — once N2 below emits the field.

## 6. NOMINATIONS

Ranked by (evidence recovered) / (risk).

### N1 — mark the interpreter's intrinsic cache (`G33-1` N2, and it is NOT in `invoke.rs`)
**File:** `vm/src/runtime/interpreter/dispatch_static.rs` (the
`CachedInvokeTarget::Intrinsic` construction in `populate_invoke_cache`) and the
matching site in `dispatch_virtual.rs`. **The brief for this lane placed this
site in `invoke.rs` and it is not there** — `invoke.rs` contains no reference to
`CachedInvokeTarget` at all; it only *calls* `populate_invoke_cache`, which
returns `()`. **Evidence:** `G33-1` §2's causal test — 100,000 `Math.abs` calls
report 1, and 100,000 with `CRATONVM_DISABLE_INTRINSICS=1`. **This is a
two-liner and the resolved id is already in scope**: `dispatch_static.rs`
resolves `(callback, native_id, native_kind)` a few lines *above* the intrinsic
probe, so the fix is
`shared.natives.native_methods.mark_invocations_incomplete(native_id);`
immediately before the `CachedInvokeTarget::Intrinsic` literal, in both files.
**This is the largest remaining gap** — it is arm-independent and therefore
invisible to the `--nojit` cross-check every lane reaches for first.

### N2 — emit `invocations_complete` in the census JSON (`G33-1` N3)
**File:** `vm/src/vm/vm_init.rs`, `dump_native_census_json`. Verified against a
real dump taken today: the writer emits row keys `class, name, descriptor, kind,
registered_by, overwrote, invocations, kind_stated, kind_chosen, owns_slot,
real_declaring_method, image_declaring_method` — **`invocations_complete` is
absent**, and the header block is `{schema_version, image_adjudication, mode,
counts, invocations, natives}` with no
`slots_with_incomplete_invocations`. **This was worth nothing before today and
is worth everything now**: with this lane's marks in, 22 rows carry the bit
and no reader can see it. One field per row plus one header number, schema bump
to 5, and `difftest/src/census.rs` reads this file. Note also that the
confirmation line still says `schema 3` while the file says
`"schema_version": 3` — `G33-1` §6 predicted this line would read 4; on
`9964ca733` both read 3, so that half of `G33-1` §6 is not in this binary.

### N3 — the three non-enumerable arms in `try_stackless_invoke`
**File:** `vm/src/runtime/interpreter/invoke.rs` (this lane's, deliberately not
done). The superclass walk, the SSL impl→API aliases and
`surefire_lazy_launcher_discover_native` resolve triples that are only known at
run time, so `UNCOUNTED_STACKLESS_NATIVES` cannot cover them. The walk arm is the
tractable one: it already holds `parent.name`, `method_name` and `descriptor` at
the `find`, so replacing that `find` with `resolve_id` + `callback_of` yields the
id at no extra hash, and the mark is one relaxed store guarded by one relaxed
load. Left undone because it edits the interpreter's hottest lookup and this lane
could not compile.

### N4 — make the exotic arms EXACT rather than merely honest
**File:** `vm/src/runtime/interpreter/invoke.rs`. §2 measures the loss at exactly
one call per site, which is small — but "small and unbounded below" is what makes
a zero unreadable. Eleven `find` calls become `resolve_id` + `callback_of` +
`record_invocation`; the documented identity
`resolve_id(..).and_then(callback_of) == find(..)` makes the lookup
cost-neutral, and the arms already pay the full `safe_native_call` funnel that
`G33-1` §5's +9.2 ns is 6% of. **Needs a real interleaved A/B on two binaries**,
14 rounds or more — the brief records a sibling lane producing a confident wrong
answer from a 6-round series.

### N5 — the JIT arm loses `Method.invoke` too, and no direct helper explains it
**File:** unknown; `vm/src/vm/vm_exec.rs` or the JIT's compiled-code native
dispatch. **Evidence:** §1 — `Method.invoke` reads **1,999** in the JIT arm
against 99,999 under `--nojit`, the same freeze-at-the-OSR-threshold signature
`HashMap.put` shows, but **there is no direct-call helper for
`Method.invoke`**. Something else in the compiled path serves it uncounted. This
is a *fourth* mechanism, unexplained, and it is the one that produces the
largest absolute loss measured in this record.

### N6 — correct `G33-1` §0, §3 and §4 in place
**File:** `docs/known-issues/jdk-only/G33-1-the-instrument-that-under-reported-20260817.md`.
§2 above supplies the amendments: the "exact configuration" headline, the
amendment to `G20-1` §8 that reversed a correct claim, and the closing
instruction in §4/§10. `G33-1` is going to be cited by every lane that reads an
`invocations` number, and "there is a configuration in which this instrument is
exact" is the sentence it puts in bold.

### N7 — `G33-1` §8 N4 is still open
**File:** `vm/src/vm/vm_exec.rs` ~25460, already documented in place: the three
`find` loops for `MethodHandle` / `VarHandle` dispatch without counting.
Unchanged by this lane — not its file — and now known to have a sibling in
`invoke.rs`, which this lane did mark. Doing both makes the
signature-polymorphic family fully declared.

### N8 — audit whether any `SyntheticStub` is reachable through a bypass
**File:** the CI gate's `invocations_of_kind(SyntheticStub) == 0`. `G33-1` §4
argued the gate has not been wrong yet because neither bypass family serves a
`SyntheticStub` — a fact about two tables' contents. There are now **four**
families (three marked or nominated here, plus N5's unexplained one), and the
`invoke.rs` family includes reflection, which reaches arbitrary triples. If a
stub is ever reachable this way the gate goes quiet rather than red. Pair the
assertion with `slots_with_incomplete_invocations()`.

## 7. What this lane did NOT do

* **Did not build, run, or test its own edits.** Everything in §4 is
  **PREDICTED**. `rustfmt --edition 2021 --check` was run on both files before
  and after and produces **exactly the same 32 and 10 pre-existing hunks** — no
  new ones — but that is formatting, not compilation, and certainly not
  behaviour. Nobody has seen `invocations_complete: false` come out of a run.
* **Did not fix `G33-1` N2**, the largest bypass, because its site is in two
  files this lane does not own (§6 N1). The brief asserted the site was in
  `invoke.rs`; it is not, and this is the single most important thing for the
  next lane to know.
* **Did not verify the regression suite against a binary containing these
  edits.** All eight vectors — `RJdkHello`, `RCollections`, `RStrings`,
  `RJdkCollections`, `RMapGcStress`, `RSyncMethodJit`, `RJitGc`,
  `RFieldSiteCache` — **pass** on `target-rel2` (`9964ca733`), which establishes
  the tree is healthy and gives the "before" side of the comparison. It does not
  establish that these edits keep it healthy.
* **Did not measure the cost of anything.** No A/B, no counter benchmark. §4's
  choice of bind-time marking rests on `G33-1` §5's numbers, which are a
  standalone `rustc -O` microbenchmark and not an A/B of two CratonVM binaries.
  That is the right order of magnitude and the wrong kind of evidence, and N4 is
  written to say so.
* **Did not explain the 1,999.** §6 N5. `Method.invoke` freezing at the OSR
  threshold with no direct-call helper in sight is a mechanism this lane did not
  isolate, and it is larger than anything this lane fixed.
* **Did not explain why `Thread.currentThread` and
  `StringLatin1.toLowerCase` read 0** in a program that certainly starts a
  thread. Marked on source-verified grounds (their helpers demonstrably do not
  count), not on a measurement that moved.
* **Did not touch `owns_slot`, `kind`, `registered_by`, `overwrote`,
  `kind_stated`, `kind_chosen`, or any count.** No record quoting those needs
  re-reading. The `invocations` numbers in this directory are unchanged; only
  what they license moves, and only in the direction of less confidence.
* **Did not edit `native-api/src/registry.rs`, `vm/src/vm/vm_exec.rs`,
  `vm/src/vm/vm_init.rs`, `vm-cli/src/main.rs`, `dispatch_static.rs`,
  `dispatch_virtual.rs`, `native_override.rs`, `INDEX.md` or `README.md`.**
  Everything outside `vm/src/jit/helpers.rs` and
  `vm/src/runtime/interpreter/invoke.rs` is a nomination.

## 8. Reproduce

```bash
JH="C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot"
CV="C:/craton/target-rel2/release/cratonvm.exe"     # 9964ca733
SP=scratchpad/g37

"$JH/bin/javac" -d "$SP" "$SP/ReflProbe.java"

# §2's decisive test. Compare `Method.invoke` across all three, and note that
# the third column is the configuration G33-1 §4 calls exact.
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 "$CV" --java-home "$JH" \
  --dump-native-registry "$SP/jit.json"   -cp "$SP" ReflProbe 100000
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 "$CV" --java-home "$JH" --nojit \
  --dump-native-registry "$SP/nojit.json" -cp "$SP" ReflProbe 100000
CRATONVM_DISABLE_INTRINSICS=1 CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 "$CV" \
  --java-home "$JH" --nojit \
  --dump-native-registry "$SP/exact.json" -cp "$SP" ReflProbe 100000
# Method.invoke: 1999 / 99999 / 99999.  HashMap.put: 0 / 100000 / 100000.

# The deficit is constant at 1, not proportional — vary n and watch it not move.
for N in 1000 10000 200000; do
  CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 "$CV" --java-home "$JH" --nojit \
    --dump-native-registry "$SP/n$N.json" -cp "$SP" ReflProbe $N
done

# Health check for the two files this lane edits — both are on the hot path
# for every call in the VM.
CV="$CV" JDK="$JH" ONLY="RJdkHello RCollections RStrings RJdkCollections \
  RMapGcStress RSyncMethodJit RJitGc RFieldSiteCache" bash regression-suite/run.sh
```

**Read the `owns_slot: true` row.** `Method.invoke` has two rows in this
binary's census — a superseded one reading 0 and the live one carrying the
count — and taking the first one that matches the triple is its own way to
conclude a body is dead.
