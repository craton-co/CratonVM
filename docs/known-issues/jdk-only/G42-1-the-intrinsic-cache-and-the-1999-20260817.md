# G42-1 — the intrinsic cache is marked, and the 1,999 is a compile-transition count that is not 1,999

**Status:** MEASURED (every number in §1–§4, taken on this host against a binary
that does **not** contain this lane's edits), SOURCE-VERIFIED (the mechanism in
§3), **PREDICTED (the code changes — this lane could not build, run or test its
own edits).** **Provenance:** every number was taken against
`C:/craton/target-rel3/release/cratonvm.exe`, release, mtime 2026-08-17 07:07,
stated to be built from `9ae371468` — the first binary carrying the coercion
instrument. `C:/craton/target-fcheck/` was ignored throughout, as briefed. The
oracle is not involved: this record is about CratonVM's own instrument.

| | |
|---|---|
| subject | `--dump-native-registry`'s `invocations` column: the interpreter's intrinsic cache, and the unexplained fourth bypass |
| predecessors | `G33-1` diagnosed the defect and built the API; `G37-1` marked two families and left the intrinsic cache and the 1,999 open |
| this lane's files | `vm/src/runtime/interpreter/dispatch_static.rs`, `vm/src/runtime/interpreter/dispatch_virtual.rs` |
| probes | `scratchpad/g42/{MixProbe,MethProbe,PutFirst,POne,PFour,PMono,PPoly,SiteProbe,ReflProbe}.java` |
| regression | `RJdkHello RCollections RStrings RJdkCollections RJdkIntrinsics3 RMethodSiteCache RFieldSiteCache RSyncMethodJit RJitGc` — **9 passed, 0 failed** |
| tree | `claude/jdk-only-mode-completion-1351c0` |

---

## 0. The headline

| finding | |
|---|---|
| the intrinsic cache now declares itself at **three** install sites, all in this lane's two files | PREDICTED |
| **the fourth mechanism is identified**: JIT-compiled code reaches `Method.invoke` through `invoke_or_native`, which resolves with `find_with_kind` and holds no id | MEASURED + SOURCE-VERIFIED, §3 |
| **the 1,999 is not a cap, not a cache-fill count and not a deopt boundary** — it is a *compile-transition count* | MEASURED, §2 |
| **the 1,999 is not 1,999.** Five identical runs gave 3999, 3999, 2999, 1999, 1999 | MEASURED, §2.4 |
| it is **not OSR-specific**: the whole-method compile route loses the count too | MEASURED, §2.3 |
| **`HashMap.put`'s "0 versus 2,000 across builds" is not a build difference — it is probe shape.** One binary produces both | MEASURED, §4 |
| the census figure silently depends on whether the *enclosing Java method* is compilable at all | MEASURED, §2.5 |
| this binary emits **`"schema_version": 4`**, not 3 | MEASURED, §6 N1 |

**The single most useful sentence in this record:** `invocations` in the JIT arm
does not measure how often a native was called — it measures **how long the
enclosing Java method stayed interpreted**, so the same program on the same
binary yields a different number run to run, and a *larger* workload yields a
*smaller* fraction counted.

---

## 1. The two marks — PREDICTED

Both files, and only these files. `invoke.rs`, `vm/src/jit/helpers.rs`,
`native-api/src/registry.rs` and `vm/src/vm/vm_init.rs` were not touched; they
are nominations (§6).

### The helpers (`dispatch_static.rs`, beside `INTRINSIC_HITS`)

* **`mark_intrinsic_cache_bypass(&NativeMethodRegistry, NativeMethodId)`** —
  `#[cold]`, one relaxed store, for the site that already holds an id.
* **`mark_intrinsic_cache_bypass_by_triple(&NativeMethodRegistry, class, name,
  desc) -> bool`** — `#[cold]`, one `resolve_id` then the same store, for the
  two sites that hold only a triple. Returns whether a row existed to mark.

**Bind time, never per call**, as briefed. `G33-1` §5 measured a hot-path
counter at **+9.2 ns/call** against a 1.25 ns baseline; an `Intrinsic` entry
exists precisely to skip the `RwLock`, the descriptor parse and the registry
probe, so that is plausibly its entire margin. This lane could not build a
binary and therefore could not run the interleaved A/B the brief requires before
choosing the per-call option — and the brief is explicit that a lane produced a
confident wrong answer from 6 rounds that 14 rounds reversed. A measurement this
lane cannot take is not one it may assume.

### The three install sites

`G37-1` §6 N1 named two. There are **three**, and the third is the blindest.

| file:site | id in hand? | what it binds |
|---|---|---|
| `dispatch_static.rs`, the ordinary intrinsic probe in `populate_invoke_cache` | **yes** — `resolve_cached_native_registration` yields `(callback, native_id, native_kind)` a few lines above | the `invokestatic` intrinsic entry |
| `dispatch_static.rs`, the `Thread.onSpinWait` install | no | an intrinsic entry installed **without requiring a registered native** |
| `dispatch_virtual.rs`, the intrinsic block in `populate_invoke_cache` | no — it reaches the intrinsic through the **class store** | the `invokevirtual`/`invokeinterface`/`invokespecial` intrinsic entry |

**The brief placed the `dispatch_static.rs` site at `~:796`. That is the
`Thread.onSpinWait` branch, which has no `native_id` in scope at all.** The site
the brief *describes* — "resolves `(callback, native_id, native_kind)` a few
lines above the intrinsic probe" — is the ordinary probe ~150 lines further
down. Both are now marked, so the discrepancy costs the next lane nothing, but
the line number in the brief is wrong and the description is right.

**`Thread.onSpinWait` is worth its own line.** The matching arm in
`execute_invokestatic_cached` answers that call site *without running the
callback at all* (the JDK body is empty; HotSpot lowers it to one `PAUSE`). In
real-JDK mode there is no row to mark — that absence is the branch's whole
premise — but `native-builtins/src/lib.rs` **does** register
`java/lang/Thread.onSpinWait ()V`, so in a builtins run the row exists and reads
zero forever. That is the configuration the mark is for.

### Which row `dispatch_virtual.rs` marks, and why

The **declaring** class's, not the receiver's. The superclass walk immediately
above (`native_override_below_declaring`) has already established that nothing
is registered strictly below the declaring class — had it found one, it would
have *vetoed* the intrinsic — so the declaring triple is the only registry row
this cache fill can shadow. A receiver-keyed lookup would resolve nothing, mark
nothing, and leave the row that actually loses the calls still claiming to be a
total. That is the silent-failure direction, and it is pinned by a test.

The `resolve_id` is taken **before `drop(cm)`**, because `declaring_name`
borrows the class store. This adds no new lock interaction: registry lookups are
lock-free, and the same block already performs two of them
(`might_have_method_descriptor` and `find`) under the same read guard.

### A `false` from the by-triple helper is normal, not a failure

`String.charAt` and `String.length` are registered **nowhere** in this binary —
`G33-1` §2 checked the dump directly and found them *absent*, not zero-valued.
A record's `hashCode`/`equals` (`record_object_intrinsic`) is not keyed on a
fixed class and has no row either. A triple with no row has no `invocations`
cell that could mislead anyone, so there is nothing to mark. Asserted, not
merely commented.

### Tests

Six, in `dispatch_static.rs`'s existing `#[cfg(test)]` block and a new one in
`dispatch_virtual.rs` (that file had none):

* `marking_an_intrinsic_install_turns_its_row_into_a_floor` — the tally survives
  marking, the bit is per slot, marking is **idempotent** (this fires once per
  call site and there are many call sites per triple, plus the promoted-
  resolution cache re-publishes across threads), a `System.identityHashCode`
  control stays exact, and the doubt reaches `census()`.
* `the_by_triple_helper_marks_only_the_named_classes_row` — `Object.hashCode`
  and a subclass's own `hashCode` are two slots.
* `a_triple_with_no_registry_row_is_a_silent_no_op` — `String.charAt`,
  `Thread.onSpinWait`, the empty registry, and the foreign-handle contract. This
  runs on the inline-cache fill path for every call in the VM, so "does not
  panic" is an assertion and not a hope.
* `the_measured_1999_is_two_osr_thresholds_of_interpreted_calls` — pins
  `OSR_THRESHOLD` so a retune makes this record's headline number visibly stale
  instead of quietly wrong.
* `the_declaring_classes_row_is_the_one_that_loses_the_calls` and
  `an_intrinsic_with_no_registry_row_leaves_the_census_untouched` — the virtual
  side's row choice.

---

## 2. The fourth mechanism — MEASURED

### 2.1 It reproduces, and the first probe that failed to reproduce it is the clue

`MixProbe.java` — *n* `Method.invoke`, *n*/10 `Constructor.newInstance`, *n*
`HashMap.put`, *n* `HashMap.get`, one process, straight-line `main`:

| native (*n* = 100,000) | JIT | `--nojit` |
|---|---:|---:|
| `Method.invoke` | **1,999** | 99,999 |
| `Constructor.newInstance` | 10,000 | 9,999 |
| `HashMap.put` | **0** | 100,000 |

That is `G37-1` §1 reproduced exactly on a newer binary. But the **first** probe
this lane wrote — an identical `Method.invoke` loop in `main`, differing only in
that `main` also carried an `if/else if` chain on `args[1]` — reported
**99,999 in the JIT arm**. Same call, same count, same *n*, opposite answer.
See §2.5.

### 2.2 Varying *n*: not a cap

| *n* | `Method.invoke`, JIT | `Constructor.newInstance`, JIT |
|---:|---:|---:|
| 5,000 | **2,999** | 500 (exact) |
| 20,000 | **1,999** | 2,000 (exact) |
| 100,000 | **1,999** | 10,000 (exact) |
| 400,000 | **1,999** | 40,000 (exact) |

The count does not resume at any larger *n*, which **rules out a deopt
boundary** — a deopt would return the site to the counted interpreter path and
the number would climb again.

### 2.3 The causal test: the loop-compilation threshold, and then the method one

`CRATONVM_TIER_OSR_BACKEDGE` is the **back-edge** threshold. `G33-1` and
`G37-1` both varied `CRATONVM_JIT_THRESHOLD` — the *method-invocation*
threshold — found it inert, and concluded the knob did not move the number.
It is the wrong knob for a loop.

| `CRATONVM_TIER_OSR_BACKEDGE` | `Method.invoke`, JIT, *n*=100,000 |
|---:|---:|
| 100 | **499** |
| 250 | **499** |
| 1,000 (default) | **1,999** |
| 5,000 | **9,999** |
| `CRATONVM_JIT_OSR=0` | **99,999** — exact, identical to `--nojit` |

One variable, monotone, and it goes to exact when the mechanism is switched off.

**But it is not OSR-specific.** With OSR *disabled* and the `Method.invoke` moved
into its own small method — so the whole-method invocation counter compiles it
instead — the count freezes again, and now `CRATONVM_JIT_THRESHOLD` *is* the
live knob:

| configuration (`MethProbe`, OSR off, *n*=100,000) | counted |
|---|---:|
| default threshold (500) | 622 |
| `CRATONVM_JIT_THRESHOLD=200` | 623 |
| `CRATONVM_JIT_THRESHOLD=5000` | **5,158** |
| `--nojit` | 99,999 |

So the rule is not "OSR loses the count". It is **"a compiled frame loses the
count"**, and either compile route gets you there. The residue above each
threshold (622 against 500, 5,158 against 5,000) is compile-install latency:
the interpreter keeps counting until the artifact is actually published.

### 2.4 It is not a cache-fill count either — and it is not a constant

Four `Method.invoke` bytecode sites in **one** loop body, so four calls per
back-edge, same *n*, same everything else:

| probe | invoke sites per iteration | counted, JIT |
|---|---:|---:|
| `POne` | 1 | **1,999** |
| `PFour` | 4 | **7,999** |
| `PMono` (monomorphic receiver) | 1 | 1,999 |
| `PPoly` (polymorphic receiver) | 1 | 1,999 |

Exactly 4×. A cap, a saturating counter or a fill-limit would not scale with
calls-per-iteration at a fixed back-edge count; a transition count is the only
thing that does. **Receiver monomorphism is irrelevant** — unsurprising in
hindsight, since the row in question is `Method.invoke`'s own, not the
reflective target's.

And the headline number is **not stable**. Five identical runs, same binary,
same probe, *n*=100,000:

```
3999   3999   2999   1999   1999
```

`1000·k − 1` for k ∈ {2, 3, 4}. The freeze point is a **race** between the
interpreter's back-edge counter and the compiler's install latency, quantized to
multiples of the threshold, and this host runs several agents concurrently.
`G33-1` §2, `G37-1` §1 and this lane's brief all state the figure as fixed. It
is not. Any record that quotes 1,999 as a constant should be read as "a number
in that family, on a host under that load".

### 2.5 The census figure depends on whether the enclosing method is compilable

The one thing no reader of the dump can see. Two probes with a byte-identical
inner loop:

| `main`'s shape | `Method.invoke`, JIT |
|---|---:|
| straight-line | **1,999** |
| a `String.equals` `if/else if` mode-dispatch chain around the loop | **99,999** |

The second `main` is never compiled, so nothing is ever lost. Reproduced twice
(the lane's first probe and `SiteProbe`). **A native's `invocations` figure is
partly a property of the shape of the Java method that calls it**, which is why
two careful records disagreed about `HashMap` (§4) and why this lane's first
attempt at reproduction failed.

---

## 3. The mechanism — SOURCE-VERIFIED

Not a direct-call helper. `G37-1` §6 N5 was right that no helper serves
`Method.invoke`; the loss is one level up, in the generic compiled-call path.

1. The x64 backend emits **`jit_invoke_virtual_mic`** for a compiled
   `invokevirtual`/`invokeinterface` — not `jit_invoke_dispatch`.
2. It first tries the leaf/native **site cache**, which **refuses**:
   `site_name_is_special_cased` (`vm/src/jit/helpers.rs`) lists **`"invoke"`**.
   The refusal is cached as a permanent negative.
3. No MIC/PIC entry can ever be published for `Method.invoke` — it is a
   registered native, so there is no compiled callee — so the site falls to the
   tail arms, which are bare **`crate::vm::invoke_or_native(...)`**.
4. `invoke_or_native` (`vm/src/vm/vm_exec.rs`) resolves the native with
   **`find_with_kind`**, which yields a callback and **no `NativeMethodId`**, and
   hands it to `safe_native_call`. Nothing counts.

The code says so itself, in a comment sitting directly above the dispatch:

> `JDK-ONLY-WAVE2: this dispatch is NOT counted for the §4 census.`
> `find_with_kind` yields no `NativeMethodId` … What must replace it: a
> `find_with_kind`-shaped lookup that also returns the slot id.

**This is the fourth family, it was documented in source the whole time, and it
is the largest.** It is not reflection-specific: `invoke_or_native` is the
generic native arm for compiled code, so *every* native dispatched from a
compiled frame that misses the site cache and the direct helpers is uncounted.
`Method.invoke` is merely the one guaranteed to miss, because `"invoke"` is on
the special-case list.

**Why `--nojit` counts all 99,999:** the interpreter's cached-native target
carries the id, and `resolve_cached_native_registration` /
`revalidate_cached_native` (`native_override.rs`) both call `record_invocation`.
The compiled path never consults `thread.invoke_cache` at all.

**The arithmetic.** `2,000 interpreted calls − 1 = 1,999`. The missing 1 is the
constant reflection deficit the brief already explains: the first call at the
site takes `try_stackless_invoke`'s `NCS_METHOD_INVOKE` arm, which holds a
callback and no id. `NativeCallSite::callback` resolves an id internally and
then throws it away, so it never counts on hit or miss. That deficit is present
in **both** arms, which is why `--nojit` reads 99,999 and not 100,000.

**Why `Constructor.newInstance` does not freeze:** it reads 10,000 at *n*=10,000
in the JIT arm and 9,999 under `--nojit` — the *opposite* pattern. Its loop is
one-tenth the length in `MixProbe`, so on the runs measured it had not reached
the back-edge threshold before `main` was compiled by the *first* loop; the
calls then went through a route that did count. This lane did **not** isolate
which route that is, and the +1 relative to `--nojit` is unexplained. §7.

### What was ruled out, and how

| hypothesis | ruled out by |
|---|---|
| a cap or saturating counter | `PFour` reports 7,999 — 4× — at the same back-edge count; `record_invocation` is an unconditional relaxed `fetch_add` |
| a cache-fill count | same |
| a deopt boundary | the count never resumes, at *n* up to 400,000 |
| the JIT **site cache** counting only on miss | `CRATONVM_JIT_SITE_CACHE=off` leaves it at **1,999**; and the site cache counts on every *hit* — it is bypassed here entirely, by the `"invoke"` special case |
| a thin direct-call helper for `Method.invoke` | there is none; confirmed against `DIRECT_CALL_HELPER_NATIVES`, which correctly omits it |
| receiver monomorphism / polymorphism | `PMono` and `PPoly` both 1,999 |
| the number of distinct call sites | 4 sites give 4× the count, not a different freeze point |
| `CRATONVM_JIT_THRESHOLD` (for the **loop** route) | inert, as both prior records found — it is the method knob; `CRATONVM_TIER_OSR_BACKEDGE` is the loop one and it moves the number linearly |
| the intrinsic table | `CRATONVM_DISABLE_INTRINSICS=1` does not close it (`G37-1` §2); `Method.invoke` is not in the intrinsic table |

---

## 4. `HashMap.put`'s "0 versus 2,000" is probe shape, not build drift

The brief records this as behaviour that is "itself unstable across builds":
`G33-1` measured 2,000 on `783685c34`, `G37-1` measured 0 on `9964ca733`.

**One binary produces both**, and the variable is where the loop sits:

| probe, JIT arm, *n*=100,000 | `HashMap.put` |
|---|---:|
| `PutFirst` — the put loop is the **first** loop in `main` | **2,000** |
| `MixProbe` — the put loop is the **third**, after the invoke and ctor loops | **0** |
| either, `CRATONVM_JIT_OSR=0` | 100,000 (exact) |

By the time the third loop starts, `main` is already compiled, so **not one
`put` is ever interpreted** and the counter never moves. `G33-1`'s `PutProbe`
put it first; `G37-1`'s `ReflProbe` put it third. Both numbers were right about
their probe and neither was about the binary. Nothing changed between the two
builds; the two records simply measured different programs.

This also disposes of the "0 proves the body is dead" hazard in its sharpest
form: **zero is the *expected* reading for a hot native in a method that was
compiled before that native's loop began.**

---

## 5. Which configurations are exact, and which are floors

Stated plainly, and this is the deliverable a reader will quote. "Exact" means
*after this lane's marks land*; the marks change no number, only what a number
licenses.

| configuration | the interpreter's intrinsic table | `Method.invoke`, `Constructor.newInstance`, `MethodHandle.*`, the `JarFile`/`ZipFile` bridges | the JIT direct helpers (`HashMap.*`, `Integer.*`, …) | **every other native called from a compiled frame** | the SSL aliases, the superclass walk, `surefire_*` |
|---|---|---|---|---|---|
| JIT, intrinsics on | **floor, NOW MARKED** | floor, marked (`G37-1`) | floor, marked (`G37-1`) | **floor, UNMARKED — §3** | floor, unmarked |
| `--nojit`, intrinsics on | **floor, NOW MARKED** | floor, marked | exact (helpers unwired) | exact | floor, unmarked |
| `--nojit` + `CRATONVM_DISABLE_INTRINSICS=1` | exact | floor, marked | exact | exact | floor, unmarked |
| `--nojit` + `CRATONVM_DISABLE_INTRINSICS=1`, no reflection, no SSL, no `surefire` | exact | — | exact | exact | — |

* **There is NO exact configuration in general.** `G37-1` §0 established this and
  it survives: the reflection column is non-empty in every row, including the one
  `G33-1` §4 prescribes as exact.
* **The nearest thing to an exact one** is `--nojit` +
  `CRATONVM_DISABLE_INTRINSICS=1`, and its residual error is now fully
  enumerated: exactly **one call per reflective/exotic call site**, plus the
  three dynamic arms of `try_stackless_invoke`. That is a bounded, known,
  *small* error, and for a census whose question is "did this ever run" it is
  harmless. Take censuses there, and read the third column as "−1 per site".
* **The JIT arm is not a census at all.** §2 shows its numbers are load-dependent,
  probe-shape-dependent and inversely related to workload size. Nothing should
  ever be concluded from an `invocations` figure taken with the JIT on.
* **`owns_slot` is unaffected** and remains the authoritative answer to "which
  body would run". `HANDOFF-20260814` §4's recommendation of this dump stands.
* **No number in any existing record changes.** What changes is what they license.

---

## 6. NOMINATIONS — everything outside this lane's two files

Ranked by (evidence recovered) / (risk).

### N1 — `vm_init.rs`'s census writer cannot express any of this
**File:** `vm/src/vm/vm_init.rs`, `dump_native_census_json`. **Verified against a
real dump taken today on `9ae371468`:**

* header keys are `{schema_version, image_adjudication, mode, counts,
  invocations, natives}` — **no `slots_with_incomplete_invocations`**;
* row keys are `{class, name, descriptor, kind, registered_by, overwrote,
  invocations, kind_stated, kind_chosen, owns_slot, real_declaring_method,
  image_declaring_method}` — **`invocations_complete` is absent**;
* the writer emits **`"schema_version": 4`**, not 3. **The brief states 3 and is
  wrong for this binary** (`G37-1` §6 N2 measured 3 on `9964ca733`, so the bump
  landed between the two builds). `--help` also documents schema 4 — but its
  row-key list omits `owns_slot`, so it is stale in the other direction.

With `G37-1`'s 22 marks and this lane's three sites in, **rows carry the bit and
no reader can see it**. One field per row, one header number, schema bump to 5;
`difftest/src/census.rs` reads this file. This is now the single highest-value
nomination in the family: everything upstream of it is done and invisible.

### N2 — count `invoke_or_native`, and the whole JIT arm closes at once
**File:** `vm/src/vm/vm_exec.rs`, `invoke_or_native`'s native arm. **Evidence:**
§3 — the largest measured loss, and an in-source comment that already prescribes
the fix verbatim: replace `find_with_kind` with `resolve_id` + `callback_of` +
`kind_of_id`, then `record_invocation(id)` before `safe_native_call`. The
identity `resolve_id(..).and_then(callback_of) == find(..)` is documented, so the
lookup is cost-neutral — **one hash either way, no second probe**, which is what
the comment's objection ("a second full triple hash") was about and why that
objection does not apply to the transformation it names. `jit_invoke_virtual_mic`,
`jit_invoke_dispatch` and the bailout path all funnel here, so this is one edit
for the whole family. **Needs an interleaved A/B, 14 rounds or more**, per the
brief. If the A/B refuses it, `mark_invocations_incomplete` at the
`find_with_kind` hit is the free fallback — but that marks a very large number of
slots, which is honest and nearly useless, so measure first.

### N3 — the `1,999` is quoted as a constant in three places and is not one
**Files:** `G33-1` §2, `G37-1` §1/§6 N5, and `vm/src/jit/helpers.rs`'s doc
comment ("does not move with the workload size or with `CRATONVM_JIT_THRESHOLD`
— it is frozen at the count reached before the enclosing loop was compiled").
The *explanation* in that doc comment is correct and is the best one-line
statement of the mechanism anywhere in the tree. The word **"frozen"** is the
problem: §2.4 measured 3999/3999/2999/1999/1999 across five identical runs.
Amend to "frozen at a load-dependent multiple of the loop threshold".

### N4 — `--help` and `record_invocation`'s doc both still promise an exact configuration
**Files:** `vm-cli/src/main.rs` (`--dump-native-registry`'s long help) and
`native-api/src/registry.rs` (`record_invocation`'s doc). Both currently say
**"For a census whose `invocations` column is exact, run with `--nojit` and
`CRATONVM_DISABLE_INTRINSICS=1`"** in bold. `G37-1` §2 falsified that and this
record confirms it. §5's table is the replacement wording. This is the sentence
every lane quotes, it is in the binary's own `--help` output, and it is wrong.

### N5 — `G33-1` §0/§3/§4 still stand uncorrected in place
**File:** `docs/known-issues/jdk-only/G33-1-...md`. `G37-1` §6 N6 nominated this
and it has not been done. Add §4 of this record: `G33-1` §2's `HashMap` table is
not evidence about `783685c34` versus `9964ca733` at all.

### N6 — the reflection deficit is fixable and `G37-1` N4 costed it
**File:** `vm/src/runtime/interpreter/invoke.rs`. Eleven `find` calls become
`resolve_id` + `callback_of` + `record_invocation`; the arms already pay the full
`safe_native_call` funnel. With N2 and this, `--nojit` +
`CRATONVM_DISABLE_INTRINSICS=1` would become **genuinely exact** for the first
time — which is the only way §5's table ever gets a clean row.

### N7 — `invocations_of_kind(SyntheticStub) == 0` now has a fourth blind family
**File:** the CI gate. `G37-1` §6 N8 counted three families; §3 adds a fourth,
and it is the generic compiled-native arm, which reaches **arbitrary triples** —
not a fixed table whose contents can be inspected. `G33-1` §4's argument that no
`SyntheticStub` is reachable through a bypass was a fact about two tables'
contents; it cannot be made about `invoke_or_native`. Pair the assertion with
`slots_with_incomplete_invocations()`.

---

## 7. What this lane could NOT settle

* **Did not build, run, or test its own edits.** Everything in §1 is
  **PREDICTED**. `rustfmt --edition 2021 --check` produces exactly the
  pre-existing **9** and **10** hunks on the two files — no new ones — but that
  is formatting, not compilation, and certainly not behaviour. Nobody has seen
  `invocations_complete: false` come out of a run, and nobody can until N1.
* **Did not explain `Constructor.newInstance`.** It reads 10,000 (JIT) against
  9,999 (`--nojit`) — the JIT arm is *higher*, and higher than the true 10,000
  is not possible for a floor unless a second route also counts. Its loop is
  one-tenth the length, so it plausibly never crosses the threshold before
  `main` is compiled by the preceding loop, but that does not explain the +1.
  Not isolated. It is the one number in this family pointing the wrong way.
* **Did not measure the cost of the marks.** No A/B. The bind-time choice rests
  on `G33-1` §5, a standalone `rustc -O` microbenchmark, which is the right order
  of magnitude and the wrong kind of evidence. The marks are `#[cold]` and off
  the dispatch path, so the exposure is a `resolve_id` per cache fill at two of
  the three sites — but "should be free" is not "measured free".
* **Did not determine which route counts `Constructor.newInstance`'s 10,000**,
  nor why `Thread.currentThread` and `StringLatin1.toLowerCase` read 0 in
  programs that certainly call them (`G37-1` §7 left the same gap).
* **Did not verify the regression suite against a binary containing these
  edits.** The nine vectors pass on `9ae371468`, which is the "before" side.
* **Did not touch `owns_slot`, `kind`, `registered_by`, `overwrote`,
  `kind_stated`, `kind_chosen`, or any count.**
* **Did not edit** `invoke.rs`, `vm/src/jit/helpers.rs`,
  `native-api/src/registry.rs`, `vm/src/vm/vm_init.rs`, `vm/src/vm/vm_exec.rs`,
  `vm-cli/src/main.rs`, `INDEX.md` or `README.md`. All nominations.

---

## 8. Reproduce

```bash
JH="C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot"
CV="C:/craton/target-rel3/release/cratonvm.exe"     # 9ae371468
SP=scratchpad/g42
export CRATONVM_DISABLE_DEFAULT_WATCHDOG=1
"$JH/bin/javac" -d "$SP" "$SP"/*.java

# §2.3, the causal test neither predecessor ran. The knob is the BACK-EDGE
# threshold, not CRATONVM_JIT_THRESHOLD.
for BE in 100 250 1000 5000; do
  CRATONVM_TIER_OSR_BACKEDGE=$BE "$CV" --java-home "$JH" \
    --dump-native-registry "$SP/be$BE.json" -cp "$SP" MixProbe 100000
done
# Method.invoke: 499 / 499 / 1999 / 9999.
CRATONVM_JIT_OSR=0 "$CV" --java-home "$JH" \
  --dump-native-registry "$SP/osroff.json" -cp "$SP" MixProbe 100000
# Method.invoke: 99999, exact. HashMap.put: 100000, exact.

# §2.4, it is a transition count and not a cap: four sites, four times the count.
for P in POne PFour PMono PPoly; do
  "$CV" --java-home "$JH" --dump-native-registry "$SP/g-$P.json" -cp "$SP" $P 100000
done
# 1999 / 7999 / 1999 / 1999.

# §2.4, and it is not a constant. Run this five times.
"$CV" --java-home "$JH" --dump-native-registry "$SP/r.json" -cp "$SP" MixProbe 100000

# §4, the HashMap "build instability" is probe shape. Same binary, both numbers.
"$CV" --java-home "$JH" --dump-native-registry "$SP/pf.json"  -cp "$SP" PutFirst 100000  # 2000
"$CV" --java-home "$JH" --dump-native-registry "$SP/mix.json" -cp "$SP" MixProbe 100000  # 0

# Health check for the two files this lane edits — both are on the inline-cache
# fill path for EVERY call in the VM.
CV="$CV" JDK="$JH" SUITE=all CRATONVM_ARGS="--jdk-only" \
  ONLY="RJdkHello RCollections RStrings RJdkCollections RJdkIntrinsics3 \
        RMethodSiteCache RFieldSiteCache RSyncMethodJit RJitGc" \
  bash regression-suite/run.sh
```

**Read the `owns_slot: true` row.** `Method.invoke` has two rows in this
binary's census — a superseded one reading 0 and the live one carrying the count
— and taking the first match is its own way to conclude a body is dead.

**And `main` must be straight-line.** A mode-dispatch `if/else` chain around the
loop stops `main` compiling and the JIT arm silently reports the exact figure
(§2.5). That is what defeated this lane's first reproduction attempt.
