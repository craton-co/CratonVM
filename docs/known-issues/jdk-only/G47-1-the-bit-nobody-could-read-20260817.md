# G47-1 — the bit nobody could read, and the arm that lost 98,000 of 100,000 calls

**Status:** MEASURED (every number in §1–§4 and §7, taken on this host against a
binary that does **not** contain this lane's edits), SOURCE-VERIFIED (§5, §6),
**PREDICTED (the code changes — this lane could not build, run or test its own
edits).** **Provenance:** every number was taken against
`C:/craton/target-rel3/release/cratonvm.exe`, release, stated to be built from
`9ae371468`. `C:/craton/target-fcheck/` was ignored throughout, as briefed. The
oracle is not involved: this record is about CratonVM's own instruments.

| | |
|---|---|
| subject | the census writer that could not express the bypass bit, and `invoke_or_native`, the fourth bypass family |
| predecessors | `G33-1` built the API; `G37-1` marked 22 slots; `G42-1` marked 3 and named this arm |
| this lane's files | `vm/src/vm/vm_init.rs`, `vm/src/vm/vm_exec.rs` |
| probes | `scratchpad/g47/probe/{GProbe,CProbe,MProbe}.java` |
| regression | `RJdkHello RCollections RStrings RJdkIntrinsics3 RMethodSiteCache RFieldSiteCache RSyncMethodJit RJitGc RCrypto` — **9 passed, 0 failed** (before side) |
| tree | `claude/jdk-only-mode-completion-1351c0` |

---

## 0. The headline

| finding | |
|---|---|
| the shipping writer emits **`"schema_version": 4`**. `G42-1` §6 N1 was right; `G37-1` §6 N2's 3 was true of `9964ca733` and is not true now | MEASURED, §1 |
| the census now emits `invocations_complete` per row and `slots_with_incomplete_invocations` in the header, at **schema 5** | PREDICTED, §2 |
| **`invoke_or_native` is the route for 98,000 of the 100,000 lost calls, measured to the unit** — from a second, independent instrument | MEASURED, §3 |
| the arm is **not JIT-only**: the same probe puts 875 calls through it under `--nojit` | MEASURED, §3 |
| **"mark at bind time" does not exist at this site**, and marking costs exactly what counting costs. The brief's two options are not the options | SOURCE-VERIFIED, §4 |
| **`Constructor.newInstance`'s 10,000-versus-9,999 is solved.** It is probe shape, like `HashMap.put`. Nothing reads higher than the truth | MEASURED, §7 |
| **`G42-1` §6 N2's prescribed fix is wrong** — `kind_of_id` is not `find_with_kind`'s kind on the quirk path, and substituting it is a dispatch-semantics change under `--jdk-only` | SOURCE-VERIFIED, §5 |
| the JIT site cache's refusal list and `invoke_or_native`'s special-case arms are **the same set of method names** | SOURCE-VERIFIED, §6 |
| the schema bump **reds `regression-suite/bridge-ratchet.sh`** until a two-token edit lands outside this lane's files | §8 N1 |

**The single most useful sentence in this record:** the census's `invocations`
column now carries its own qualifier, so `0` can finally be told apart from
`at least 0` — and the largest thing that made it a floor,
`invoke_or_native`, now declares itself on every dispatch instead of on none.

---

## 1. The schema version, settled — MEASURED

Two lanes disagreed and the brief asked for a dump rather than a citation. Taken
today, `--jdk-only`, `RJdkHello`:

```
[cratonvm] wrote native registry census (schema 4, no image adjudication …)
```

```json
{ "schema_version": 4, "image_adjudication": false, "mode": "jdk-only",
  "counts": {…}, "invocations": {"intrinsic": 267, "bridge": 2818, "synthetic-stub": 0},
  "natives": [ … ] }
```

**It is 4.** Row keys, read from the file rather than from the source:

```
class, name, descriptor, kind, registered_by, overwrote, invocations,
kind_stated, kind_chosen, owns_slot, real_declaring_method, image_declaring_method
```

Header keys: `schema_version, image_adjudication, mode, counts, invocations,
natives`.

So, precisely:

* `G42-1` §6 N1 (**4** on `9ae371468`) is confirmed.
* `G37-1` §6 N2 (**3** on `9964ca733`) was correct for its binary; the bump
  landed between the two builds. Neither record was wrong; the disagreement was
  about two different binaries and neither said so.
* `--help` documents schema 4 and its row-key list omits `owns_slot` — stale in
  the other direction, still, and now stale in a second way. §8 N2.
* **`invocations_complete` is absent from every row and
  `slots_with_incomplete_invocations` from the header**, exactly as both
  predecessors reported. 25 slots were carrying a bit that nothing on the planet
  could read.

## 2. What the dump emits now — PREDICTED

`vm/src/vm/vm_init.rs`, `dump_native_census_json_with`. Purely additive: two
keys, one constant, two small pure functions, and **no change to any number**.

**Header**, between `invocations` and `natives`:

```json
  "invocations": { "intrinsic": 267, "bridge": 2818, "synthetic-stub": 0 },
  "slots_with_incomplete_invocations": 25,
  "natives": [
```

**Row**, immediately after the tally it qualifies:

```json
      "invocations": 1999,
      "invocations_complete": false,
```

Schema **5**, stamped from a named constant
(`NATIVE_CENSUS_SCHEMA_VERSION`) rather than a literal, because three records
and one `--help` string have now disagreed about this integer and every one of
them was reading a different source of truth.

Two deliberate choices worth stating, because a reader will otherwise infer
them wrongly:

* **The header number counts slots; `natives` is one row per registration.** So
  it is *not* the number of rows carrying `false` — a superseded row shares its
  successor's slot and shows the same bit. That asymmetry is inherited from
  `counts` (registrations) versus `invocations` (slots) and is deliberate in
  both.
* **The two row fields are emitted by one function that cannot emit either
  alone.** The entire defect this record closes is that the tally was readable
  without its qualifier; the structural fix is that a future edit has to delete
  the qualifier on purpose. Pinned by a test.

`difftest/src/census.rs` **needs no change to keep working**: it locates the row
array by scanning for objects carrying `kind`, reads `invocations` per row, and
tolerates unknown keys by construction ("shape-tolerant and never fatal"). It
should nonetheless *learn* the bit — §8 N3.

## 3. `invoke_or_native` accounts for 98,000 of the missing 98,000 — MEASURED

`G42-1` §3 identified the mechanism from source and measured its consequence in
the native census. This lane measured it a second time, from an **independent
instrument** — `CRATONVM_DBG_NATIVE_LOOKUPS=1`, the per-invoke lookup census
that already sits at the top of `invoke_or_native` — and the two agree to the
unit.

`GProbe.java`: straight-line `main`, *n* reflective `Method.invoke`, then *n*
`HashMap.put`. *n* = 100,000, same binary, same host:

| | JIT | `--nojit` |
|---|---:|---:|
| `invokes(general)` — calls reaching `invoke_or_native` | **98,875** | **875** |
| `find_with_kind` | 101,168 | 3,171 |
| `resolve_id` | 384 | 387 |
| census `Method.invoke` | **1,999** | **99,999** |
| census `HashMap.put` | **0** | **100,000** |

Now the arithmetic, which is the point:

```
general-resolver traffic added by the JIT arm:  98,875 − 875 = 98,000
census calls lost by the JIT arm:            99,999 − 1,999 = 98,000
```

**Identical.** Every call the census loses in the JIT arm is a call that went
through `invoke_or_native`, and there is no residue for a fifth mechanism to
hide in. `G42-1` §3's source reading is now confirmed by measurement, and the
family is closed rather than merely named.

Two things fall out that no predecessor stated:

1. **It is not a JIT family.** The `--nojit` arm still puts **875** calls through
   this resolver in the same program. `invoke_or_native` is called from
   `dispatch_static.rs`, `lambda.rs`, `agent_loader.rs` and `debug/mod.rs` as
   well as from the JIT's tail arms. The `--nojit` census is not exact here — it
   is *nearly* exact, by a factor of about 113 on this probe, which is why three
   records in a row have been able to treat it as a control.
2. **`resolve_id` is currently 384 on this path.** That is the A/B baseline for
   §4: today essentially nothing on the general resolver resolves an id.

## 4. The decision, and why the brief's two options were not the options

The brief framed it as **mark-at-bind (free) versus count-per-call (exact, +9.2
ns)**. Reading the site settles it differently, and this is the one thing about
it that could be established without a build.

**There is no bind point in `invoke_or_native`.** `G37-1` and `G42-1` both had
one — a JIT call site being wired to a direct helper, an intrinsic inline-cache
fill — where one cold store buys silence forever. This function wires nothing:
it re-resolves the triple on every call. So "mark once at bind" does not exist
here; the cheapest honest thing available is "mark on dispatch".

**And a mark needs a `NativeMethodId`, which means `resolve_id` — the same
lookup a count needs.** `mark_invocations_incomplete` takes an id;
`find_with_kind` yields none. So the two options cost the *same lookup* and
differ only by a relaxed `fetch_add` versus a relaxed `store`. Marking is not
the free option at this site; it is the same-price, less-informative one.
`G42-1` §6 N2's own fallback — "`mark_invocations_incomplete` at the
`find_with_kind` hit is the free fallback" — is not free.

### What was implemented

**Default: declare, at most once per (thread, VM, registry generation,
callback).** A two-entry thread-local memo, the same `Cell`-of-`Copy` shape
`PERMISSIVE_DISPATCH_MEMO` uses eight lines away, turns the steady state — a
loop calling the same native, which is exactly the measured shape — into a
thread-local load and two compares. `resolve_id` is paid on the first dispatch
of a callback and not again.

**On request (`CRATONVM_CENSUS_EXACT_INVOCATIONS`): count exactly**, one
`resolve_id` plus one relaxed `fetch_add` per dispatch. Off by default because
nobody has run the interleaved A/B the brief requires; on when an operator asks,
which is the run where the number is the entire point.

### The cost, bounded from a measurement rather than assumed

From §3, on the hottest case anyone has produced (100,000 reflective calls in a
compiled loop):

| | `resolve_id` calls added | at `G33-1` §5's order of magnitude |
|---|---:|---|
| unconditional count (the option the brief offered) | **+98,000** | ~1–4 ms on a run whose native boundary is ~14 ms — 10–25% of the boundary |
| this lane's default arm | **+1** | unmeasurable |
| this lane's exact arm, when asked | +98,000 | the same as the first row, on request |

And in a whole `--jdk-only` regression vector the arm is not hot at all:
`invokes(general)` reads **913** (`RJdkHello`), **1,113** (`RCollections`),
**831** (`RJitGc`) for entire programs. The default arm's cost there is a
handful of `resolve_id` calls per run, total.

This is the same trade the capability gate ~16,300 lines above already argues
for on this same path and for this same reason — "re-deriving it with
`resolve_id` would pay a *second* full 128-bit hash per dispatch … the id is
resolved only after step 2 has said this native is capability-relevant, in the
`#[cold]` half" — and it inherits that gate's stated honesty: the default arm
**under-reports and says so** rather than reporting a number it cannot stand
behind.

**What was NOT done, and it is the right fix:** the in-source comment at the
`find_with_kind` hit prescribes "a `find_with_kind`-shaped lookup that also
returns the slot id". That is one `native-api` function and it makes this arm
exact for **zero** extra hashes. §8 N4.

### Where the hook sits

Fourteen call sites, all in `invoke_or_native`, each one statement immediately
before an existing `safe_native_call` and after the capability gate — "the last
point before the native actually runs", the placement that function's own
comment argues for, so a call the arms above route to real bytecode is never
declared to have bypassed anything. A capability refusal returns before the
hook, and is therefore not counted, which is correct: the native did not run.

### The guard

The resolved slot's callback is compared **by address** against the one about to
run, and nothing is recorded if they differ. `resolve_id` and `find_with_kind`
agree on the fast exact-hash path by construction but reach their
descriptor-quirk fallbacks through *different* functions. A wrong number on a row
that looks authoritative is worse than the silence it replaced.

## 5. `G42-1` §6 N2's prescription is wrong, and would have moved dispatch

Worth its own section because it is the fix two records have now recommended and
a third would have applied.

`G42-1` §6 N2: *"replace `find_with_kind` with `resolve_id` + `callback_of` +
`kind_of_id`"*. The first two are fine. **`kind_of_id` is not `find_with_kind`'s
kind.** `find_with_kind`'s own doc records that its cold descriptor-quirk arm
looks the kind up with the **original** descriptor, misses, and *deliberately*
falls back to `Bridge`:

> the kind is looked up with the ORIGINAL (un-rewritten) descriptor, so it
> misses and falls back to `Bridge`. The slot now carries its true kind and we
> could report it exactly, but that would change which natives the real-JDK
> `SyntheticStub` drop applies to on quirky descriptors — a dispatch semantics
> change, out of scope for a perf change. Left as-is, deliberately.

`kind_of_id` returns the slot's true kind. On a quirky descriptor that flips
`synthetic_stub_native`, which feeds `real_protected_stub` and
`resolve_native_dispatch_wave1` — and under `--jdk-only`,
`resolve_native_dispatch_wave1` can **reject**. That is a dispatch-semantics
change, in the mode all 99 regression vectors run in, shipped inside a change
whose stated purpose is measurement-only.

`find_with_kind` is therefore untouched here. The correct identity, for whoever
writes §8 N4, is:

```
find_with_kind(c,m,d)  ≡  (find(c,m,d)?, kind_of(c,m,d).unwrap_or(Bridge))
                       ≡  (resolve_id(c,m,d).and_then(callback_of)?, kind_of(c,m,d).unwrap_or(Bridge))
```

— which is two hashes, which is why `find_with_kind` exists, which is why the
new function has to be written rather than composed.

## 6. The refusal list and the special-case arms are the same set

`site_name_is_special_cased` (`vm/src/jit/helpers.rs`) — the list that makes the
JIT site cache refuse a site permanently — is:

```
type, invoke, invokeExact, invokeBasic, apply, loadClass,
setDefaultAssertionStatus, getResource, getResources, getResourceAsStream,
isMultiRelease, getJarEntry, getRealName, getCertificates, getCodeSigners,
getEntry, bufferEndsWithSignatureSuffix, <init>, <clinit>
```

Every one of those names is a name `invoke_or_native` special-cases in a
constant-triple arm of its own — the `DowncallHandle` pair, the three
`ClassLoader` arms, the five Spring-Boot loader arms. **The set the site cache
refuses and the set this lane instrumented are the same set**, which is why all
fourteen arms were hooked rather than only the generic one: a name on that list
is a name whose every compiled call site is guaranteed to land here forever.

`newInstance` is **not** on the list. That is §7.

## 7. `Constructor.newInstance`'s 10,000 — solved, and nothing reads too high

`G42-1` §7 left this as "the one number in this family pointing the wrong way":
10,000 in the JIT arm against 9,999 under `--nojit`, and *"higher than the true
10,000 is not possible for a floor unless a second route also counts"*.

It is probe shape, exactly as `HashMap.put`'s 0-versus-2,000 was (`G42-1` §4).
Two probes, one binary, `n`/10 = 10,000 `newInstance` calls in both:

| probe | where the ctor loop sits | JIT | `--nojit` |
|---|---|---:|---:|
| `CProbe` | the **only** loop in `main` | **9,999** | 9,999 |
| `MProbe` | the **second** loop, after an `invoke` loop | **10,000** | 9,999 |

And the sanity rows: *n* = 0 reads 0 in both arms, *n* = 1 reads **0** in both
arms — the constant −1, reproduced at the smallest possible workload.

The mechanism, and it needs both halves of this record:

* The **−1** is paid only when the *first* call at the site is served by the
  interpreter's `try_stackless_invoke` `NCS_CONSTRUCTOR_NEW_INSTANCE` arm, which
  holds a callback and no id (`G37-1` §2).
* `"newInstance"` is **not** on `site_name_is_special_cased` (§6), so once the
  enclosing method is compiled the JIT **site cache** serves it — and the site
  cache **counts on every hit, including the first**.
* In `MProbe` the `invoke` loop compiles `main` before the ctor loop starts, so
  the very first `newInstance` never touches the interpreter's stackless arm. No
  call is lost, and 10,000 is the **true** count.

So nothing reads higher than the truth. `--nojit`'s 9,999 is the short number,
not the JIT arm's 10,000, and the two arms lose *different* calls. `G42-1` §7's
"the one number pointing the wrong way" was pointing the right way at a
different question.

## 8. NOMINATIONS — everything outside this lane's two files

Ranked by (evidence recovered) / (risk). **N1 is a CI obligation, not an
improvement.**

### N1 — the schema bump reds the bridge ratchet until two tokens move
**Files:** `scripts/jdk-only-bridge-ratchet.py` (`REQUIRED_CENSUS_SCHEMA = 4`,
line 121) and `scripts/baselines/jdk-only-bridge-ratchet.json`
(`"census_schema_version": 4`, line 25). The gate tests
`block["census_schema_version"] != REQUIRED_CENSUS_SCHEMA` — **equality, not
`>=`** — and returns 2 with "REFUSING: … Only schema 4 carries
image_declaring_method, which is the whole question this gate asks." Schema 5 is
a strict superset and still carries it; the pin is over-strict, and
`regression-suite/bridge-ratchet.sh` runs the gate. **Both values must become 5
in the same wave as this change, or `bridge-ratchet.sh` goes red on a file that
is strictly more informative than the one it accepted.** This was chosen with
eyes open: the alternative — new keys under an unchanged `schema_version` — is
the "one `schema_version` with two shapes" hazard `dump_native_census_json`'s
own doc is written against. `scripts/jdk-only-kind-map.py` asks for `>= 2` and
needs nothing; `scripts/jdk-only-adjudicate.py` only prints the value;
`tools/jdk-only-blockers/blockers.py` documents "schema_version 2" in a comment.

### N2 — `--help` is now stale in three ways
**File:** `vm-cli/src/main.rs`, `--dump-native-registry`'s long help (~504) and
the confirmation line's `schema 4` literal (~2816–2823). It documents schema 4,
its row-key list omits **`owns_slot`** (already stale, `G42-1` §6 N1), and it
will now also omit `invocations_complete` and
`slots_with_incomplete_invocations`. It additionally still says in bold **"For a
census whose `invocations` column is exact, run with `--nojit` and
`CRATONVM_DISABLE_INTRINSICS=1`"** — falsified by `G37-1` §2, and §3 above adds a
second falsifier (875 general-resolver calls in the `--nojit` arm of one probe).
This is the sentence every lane quotes and it is printed by the binary itself.
`record_invocation`'s doc in `native-api/src/registry.rs` carries the same
sentence and the same obligation.

### N3 — teach `difftest` the bit
**File:** `difftest/src/census.rs`, `parse_native_invocations`. It keeps working
unchanged (§2), which is the *hazard*: it will keep summing a column that is now
labelled a floor and reporting the sum as a total on the ledger. The
shape-tolerant parser makes this a small addition — read
`invocations_complete` per row and carry a `native_invocations_incomplete` count
onto `StrictCensus`, so a divergence report can say "these totals are floors"
instead of implying they are not. Nothing in that file needs to change for
correctness today; it needs to change for honesty.

### N4 — `find_with_kind_and_id`, and the whole arm becomes exact for free
**File:** `native-api/src/registry.rs`. One function with `find_with_kind`'s
*exact* semantics — fast path returns `(slot.callback, slot.kind,
Some(NativeMethodId))`, quirk path returns `(cb, Bridge, quirk id)` — lets
`invoke_or_native` count every dispatch for **zero** extra hashes and lets this
lane's memo and env gate be deleted. It also collapses
`check_native_dispatch_capability`'s step 2 into the precomputed
`sensitive_slots` lookup, which that function's doc has been asking for
separately. Two callers, one function, no new hash anywhere. **Do not compose it
from `resolve_id` + `callback_of` + `kind_of_id` — see §5.**

### N5 — `G33-1` §0/§3/§4 and `G37-1` §6 N5 still stand uncorrected
**Files:** `G33-1-…md`, `G37-1-…md`. `G37-1` §6 N6 and `G42-1` §6 N5 both
nominated the `G33-1` corrections and neither has been done. §3 above closes
`G37-1` §6 N5 ("the JIT arm loses `Method.invoke` too, and no direct helper
explains it") with a measurement, and §7 closes `G42-1` §7's
`Constructor.newInstance` gap; both should be marked resolved in place so the
next lane does not re-derive them.

### N6 — the CI gate's fourth blind family is now the one that can be told
**File:** the `invocations_of_kind(SyntheticStub) == 0` gate. `G37-1` §6 N8 and
`G42-1` §6 N7 both nominated pairing it with
`slots_with_incomplete_invocations()`. As of schema 5 that number is **in the
file the gate already reads**, so the pairing is now a one-line change on the
consumer side rather than a request for new plumbing.

### N7 — the three non-enumerable `try_stackless_invoke` arms
**File:** `vm/src/runtime/interpreter/invoke.rs`. Unchanged: `G37-1` §6 N3's
superclass walk, SSL impl→API aliases and `surefire_lazy_launcher_discover_native`
still resolve run-time triples and stay silent floors. This lane's §4 argument
applies to the walk arm too — it already holds the triple, so a mark and a count
cost the same lookup, and the count is worth more.

---

## 9. Which configurations are exact, and which are floors

"Exact" means *after this lane's marks land*; the marks change no number, only
what a number licenses. Compare with `G42-1` §5 — the change is the fourth
column, and the honest overall answer is still **all of them are floors, with
one fewer hole**.

| configuration | intrinsic table | `Method.invoke`, `MethodHandle.*`, `JarFile`/`ZipFile` bridges | JIT direct helpers (`HashMap.*`, `Integer.*`, …) | **every native through `invoke_or_native`** | SSL aliases, superclass walk, `surefire_*` |
|---|---|---|---|---|---|
| JIT, intrinsics on | floor, marked (`G42-1`) | floor, marked (`G37-1`) | floor, marked (`G37-1`) | **floor, NOW DECLARED — or exact with `CRATONVM_CENSUS_EXACT_INVOCATIONS`** | floor, unmarked |
| `--nojit`, intrinsics on | floor, marked | floor, marked | exact (helpers unwired) | **floor, NOW DECLARED** — §3 measured 875 calls, it is not zero | floor, unmarked |
| `--nojit` + `CRATONVM_DISABLE_INTRINSICS=1` | exact | floor, marked | exact | **floor, NOW DECLARED** | floor, unmarked |

* **There is still no exact configuration**, and §3 removes the last excuse for
  believing otherwise: the `--nojit` arm has its own general-resolver traffic.
  What changed is that a reader can now *see* which rows are affected instead of
  having to know that three records exist.
* **`CRATONVM_CENSUS_EXACT_INVOCATIONS=1` closes the fourth column** and, with
  N7, `--nojit` + `CRATONVM_DISABLE_INTRINSICS=1` +
  `CRATONVM_CENSUS_EXACT_INVOCATIONS=1` would be the first genuinely exact
  configuration this directory has been able to name.
* **The JIT arm is still not a census.** `G42-1` §2 stands: its numbers are
  load-dependent, probe-shape-dependent and inversely related to workload size.
  §7 adds a third species of that hazard, on a native nobody suspected.
* **`owns_slot` is unaffected.** `HANDOFF-20260814` §4's recommendation of this
  dump stands, and is now better founded.
* **No number in any existing record changes.**

## 10. Risk, and which green vectors reach what was touched

`vm_exec.rs` is the hot path for every call in the VM and `vm_init.rs` runs at
every startup. The change is **780 insertions and 4 deletions** —
`vm_exec.rs` is **542 insertions and zero deletions**, purely additive, the
shape a previous lane in this family said made it safe. All four deletions are
in `vm_init.rs` and all four are the census writer's string emission: the doc
example's `schema_version` line, the writer's `schema_version` literal, the
`"  },\n  \"natives\": ["` header line, and the single-line `invocations` row
emit. Every behavioural addition is a statement that returns `()`, reads the
registry, writes only census state, and cannot change which callback runs.

| vector | reaches | why it is the right witness |
|---|---|---|
| `RJdkHello` | `dump_native_census_json_with`, the general resolver | the census dump in §1 was taken from it; 913 general-resolver calls |
| `RCollections` | the general resolver, hard | 1,113 general-resolver calls, the highest of the three sampled |
| `RStrings` | the `apply`/`invoke` special-case arms | string SAM bridges are on the refusal list of §6 |
| `RJdkIntrinsics3` | the intrinsic table beside the marked installs | the arm `G42-1` edited; proves the two lanes compose |
| `RMethodSiteCache` | `jit_invoke_virtual_mic`'s site cache and its refusals | the exact mechanism of §3, exercised deliberately |
| `RFieldSiteCache` | the same cache's field half | the negative control for the above |
| `RSyncMethodJit` | compiled frames calling natives under a monitor | the memo is thread-local; this is the vector with contention |
| `RJitGc` | compiled frames plus relocation | 831 general-resolver calls with the collector moving under them |
| `RCrypto` | the `ClassLoader` and JCA provider arms | the deepest constant-triple special-case traffic in the suite |

All nine **pass** on `9ae371468`, which is the *before* side. It does not
establish that these edits keep them passing.

---

## 11. What this lane did NOT do

* **Did not build, run, or test its own edits.** Everything in §2 and §4 is
  **PREDICTED**. `rustfmt --edition 2021 --check` produces exactly the
  pre-existing **16** and **45** hunks on the two files — no new ones, verified
  against the `HEAD` copies of both — but that is formatting, not compilation.
  Nobody has yet seen `invocations_complete: false` come out of a run.
* **Did not run the interleaved A/B.** §4's cost table is a `resolve_id`-call
  count taken from a real instrument multiplied by `G33-1` §5's standalone
  microbenchmark. That is the right order of magnitude and the wrong kind of
  evidence, which is exactly why the exact arm is opt-in and not the default.
* **Did not change `find_with_kind`, `resolve_native_dispatch_wave1`, the
  capability gate, or any dispatch decision.** §5 is the reason, and it is a
  reason two predecessors' recommended fix would have run into.
* **Did not cover `try_stackless_invoke`'s three dynamic arms** (§8 N7) or
  anything in `invoke.rs`, `helpers.rs`, `dispatch_*.rs` — not this lane's files.
* **Did not move `scripts/jdk-only-bridge-ratchet.py`.** §8 N1. The gate is red
  until someone does, and this record says so in §0 rather than leaving it to be
  discovered by CI.
* **Did not touch `owns_slot`, `kind`, `registered_by`, `overwrote`,
  `kind_stated`, `kind_chosen`, or any count.**
* **Did not edit** `native-api/src/registry.rs`, `vm-cli/src/main.rs`,
  `difftest/src/census.rs`, `dispatch_static.rs`, `dispatch_virtual.rs`,
  `vm/src/jit/helpers.rs`, `INDEX.md` or `README.md`. All nominations.

## 12. Reproduce

```bash
JH="C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot"
CV="C:/craton/target-rel3/release/cratonvm.exe"     # 9ae371468
SP=scratchpad/g47
export CRATONVM_DISABLE_DEFAULT_WATCHDOG=1
"$JH/bin/javac" -d "$SP/probe" "$SP/probe"/*.java

# §1. Settle the schema version from the file, not from a record.
"$CV" --java-home "$JH" --jdk-only --dump-native-registry "$SP/before.json" \
  -cp regression-suite/build RJdkHello
python -c "import json;d=json.load(open(r'$SP/before.json'));print(d['schema_version'],list(d.keys()));print(list(d['natives'][0].keys()))"

# §3, the decisive one. The lookup census and the native census agree to the unit:
#   (98875 − 875) == (99999 − 1999) == 98000.
for M in "" "--nojit"; do
  CRATONVM_DBG_NATIVE_LOOKUPS=1 "$CV" --java-home "$JH" $M -cp "$SP/probe" GProbe 100000 \
    2>&1 | grep native-lookups
  "$CV" --java-home "$JH" $M --dump-native-registry "$SP/g$M.json" -cp "$SP/probe" GProbe 100000
done

# §7. The ctor +1 is probe shape. Same binary, same n, both numbers.
"$CV" --java-home "$JH" --dump-native-registry "$SP/c.json" -cp "$SP/probe" CProbe 100000  # 9999
"$CV" --java-home "$JH" --dump-native-registry "$SP/m.json" -cp "$SP/probe" MProbe 100000  # 10000
# And the constant −1 at the smallest workload: n=1 reads 0 in BOTH arms.

# Health check for the two files this lane edits.
CV="$CV" JDK="$JH" SUITE=all CRATONVM_ARGS="--jdk-only" \
  ONLY="RJdkHello RCollections RStrings RJdkIntrinsics3 RMethodSiteCache \
        RFieldSiteCache RSyncMethodJit RJitGc RCrypto" \
  bash regression-suite/run.sh
```

**Read the `owns_slot: true` row.** `Method.invoke` has two rows in this
binary's census — a superseded one reading 0 and the live one carrying the count.

**And `main` must be straight-line** (`G42-1` §2.5), **and the loop you care
about must be the first one** (`G42-1` §4 and §7 above). Two of the three
"unexplained" numbers in this family turned out to be the second rule.
