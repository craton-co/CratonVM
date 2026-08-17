# G33-1 — the instrument that under-reported, and the configuration in which it does not

**Status:** MEASURED (diagnosis, causal), SOURCE-VERIFIED (mechanism),
**PREDICTED (the code changes — this lane could not build or run its own
edits).** **Provenance:** every number below was taken on this host against
`C:/craton/target-rel/release/cratonvm.exe`, release, mtime 2026-08-17 02:09,
`cratonvm.d` source root `C:\craton\cvm-mergecheck`, stated to be built from
`783685c34`. The oracle is not involved — this record is about CratonVM's own
instrument, and HotSpot has no opinion on it.

| | |
|---|---|
| subject | `--dump-native-registry`'s `invocations` column |
| why it matters | `HANDOFF-20260814` §4 recommends this dump as the way to settle "which body runs"; a dozen records in this directory quote its output |
| probes | `scratchpad/probe/{CountProbe,PutProbe,FamProbe,IntrProbe}.java`, `scratchpad/bench_counter.rs` |
| host | this Windows 11 box, several agents running concurrently throughout |
| tree | `claude/jdk-only-mode-completion-1351c0`, working tree dirty (other lanes mid-flight) |

---

## 0. The headline

| finding | |
|---|---|
| **the counter is exact in `--nojit` with intrinsics off** | MEASURED, every native probed |
| the two mechanisms that lose calls | the interpreter's **intrinsic table**; the JIT's **thin direct-call helpers** |
| causal proof of mechanism 1 | 100,000 `Math.abs` calls report **1**; identical run with `CRATONVM_DISABLE_INTRINSICS=1` reports **100,000** |
| causal proof of mechanism 2 | 100,000 `HashMap.get` calls report **1,873** with the JIT on, **100,001** under `--nojit`; the JIT figure does not move with workload size or `CRATONVM_JIT_THRESHOLD` |
| **G20-1 §8's "it under-reports in the `--nojit` arm too" is NOT reproducible** | `--nojit` was exact for every registry-dispatched native measured here |
| cost of making the count true on the hot path | **+9.2 ns per call** — ~6% of the 141 ns native boundary, but *more than the entire margin* a thin direct-call helper exists to buy |
| the misplaced-flag family | was silent; now warns per flag, names the fix, suppressible |
| `invocations` field meaning | **RESTATED — see §4, it changes how a dozen records should be read** |

**The single most useful sentence in this record:** there is a configuration in
which this instrument is exact, and it is `--nojit` plus
`CRATONVM_DISABLE_INTRINSICS=1`. Take censuses there.

---

## 1. What `invocations` actually counts — SOURCE-VERIFIED

`NativeMethodRegistry::record_invocation` (`native-api/src/registry.rs`) is one
bounds-checked index and one relaxed `fetch_add`. It is **not** sampled, **not**
saturating, **not** behind a `cfg` or a flag, **not** reset by re-registration,
and **not** per-registration. Every one of the brief's candidate explanations is
false. The function is correct and cheap and does exactly what its doc says.

The defect is not in the counter. It is in **what the counter is attached to.**

`record_invocation` takes a `NativeMethodId`. A caller can only have one if it
resolved the triple through the registry. Every performance optimisation this VM
has made to the native path consists of *removing that resolution from the hot
path* — resolve once, cache the function pointer, call it directly forever
after. Each such optimisation took the counter with it, silently, because
nothing in the type system says "this call site owes the census a tick".

So `invocations` is an exact count of **registry-resolved dispatches** and a
**lower bound on Java-level calls**. Those two quantities were the same thing
when the counter was written. They are not the same thing now.

## 2. The two mechanisms, isolated — MEASURED

### Mechanism 1 — the interpreter's intrinsic table (arm-independent)

The invoke caches install a `CachedInvokeTarget::Intrinsic` holding a raw `fn`
pointer; the registry is never consulted again for that site. This is the
mechanism G20-1 could not find, and it is the one that matters, because **it is
just as blind under `--nojit`.**

Decisive causal test — one probe, four configurations, 100,000 calls of each
kernel, `IntrProbe.java`:

| native (100,000 calls each) | `--nojit` | `--nojit`, intrinsics OFF | JIT | JIT, intrinsics OFF |
|---|---:|---:|---:|---:|
| `Math.abs` | **1** | **100,000** | **1** | 2,000 |
| `Object.hashCode` | **1** | **100,000** | 97,001 | 100,000 |
| `System.identityHashCode` (control) | 100,000 | 100,000 | 100,000 | 100,000 |
| `String.charAt` | 0 | 0 | 0 | 0 |
| `String.length` | 0 | 0 | 0 | 0 |

`CRATONVM_DISABLE_INTRINSICS=1` moves `Math.abs` from 1 to 100,000 **in the
interpreter arm**. That is the causal proof, and it is a clean one: one variable,
one probe, one binary, a control that does not move.

**The `String.charAt` zeros are honest.** There is **no registry row at all** for
`java/lang/String.charAt` or `.length` in this binary's census — checked
directly in the dump. Nothing is registered, so nothing can be counted. G20-1 §5
reasoned from `Preconditions.checkIndex(...) = 1` on a 1,000,000-call `charAt`
loop and its timing check was right for the right reason: `charAt` never enters
Rust. That conclusion stands; only its supporting claim about the *counter* was
mis-attributed.

`Integer.valueOf` / `Integer.intValue` are the same story from the same table:
100,000 calls report **2** and **1** respectively in **both** arms. This is the
autobox latch G20-1 §7 measured as "provably always armed", seen from the census
side. Honest, and invisible.

### Mechanism 2 — the JIT's thin direct-call helpers (JIT arm only)

`jit::try_compile` recognises a small set of hot triples and emits a direct
`CALL` to a VM-side helper — `HASHMAP_PUT_DIRECT_FN` and siblings
(`jit/src/lib.rs`, the `direct_native_helper` block). The helper
(`vm/src/jit/helpers.rs`, `jit_hashmap_put_direct` / `jit_hashmap_get_direct`)
open-codes the native's semantics through `jit_overlay_hashmap_put/get` and
returns **with no `NativeMethodId` anywhere in scope**. Only the slow fallback at
the bottom, which re-enters `jit_invoke_dispatch`, counts.

MEASURED, `PutProbe.java`, `HashMap.put`, distinct keys:

| n | JIT arm | `--nojit` arm |
|---:|---:|---:|
| 1,000 | 1,000 | 1,000 |
| 5,000 | **2,000** | 5,000 |
| 50,000 | **2,000** | 50,000 |
| 1,000,000 | **2,000** | — |

The JIT figure freezes at the count reached before the enclosing loop was
compiled, and **does not move** with `CRATONVM_JIT_THRESHOLD` at 50, 500 or
5,000 (that knob is the method-invocation threshold, not the OSR back-edge one).
`HashMap.get` behaves identically (1,873 against 100,001).

**This family is inconsistent with itself, and that is the tell.** The `Integer`
siblings in the same file route through `call_integer_native_raw`, whose doc is
explicit that its open-coded arms *must* be counted — *"not counting them would
leave exactly the kind of unverifiable path the acceptance criterion is written
against"*. The `HashMap` helpers were added later (`perf/halfgap-20260717`) and
never got that wrapper. This is an omission, not a design decision.

Controls that count exactly in **both** arms, from `FamProbe.java` at 100,000
calls: `System.identityHashCode` 100,000; `ConcurrentHashMap.get` 100,064;
`StringLatin1.toLowerCase` 100,000. The last one is worth noting — it has a
direct helper too, and it counted, so the direct-call family is not uniformly
broken.

## 3. Corrections to G20-1 §8

G20-1 is a careful record and its §8 is honestly labelled "what was measured and
could not be explained". Two of its three claims about this counter need
amending, and one is confirmed.

1. **"It under-reports in the `--nojit` arm too, so it is not simply the JIT
   bypassing the counter."** The conclusion is **right** and the evidence for it
   was **wrong**. `--nojit` is exact for every registry-dispatched native
   measured here. But there *is* an arm-independent mechanism — the intrinsic
   table — and it is the bigger one. G20-1 reached a true conclusion from
   `Preconditions.checkIndex = 1`, which was not an instrument failure at all.
2. **"The `--nojit` top-10 table is byte-identical to the JIT arm's."** Not
   reproducible. In every paired run here the two arms differ on exactly the
   natives with direct-call helpers.
3. **"`owns_slot` is still authoritative."** Confirmed. Nothing found here
   touches `owns_slot`, `kind`, `registered_by`, `overwrote`, `kind_stated` or
   `kind_chosen`. **Only `invocations` is affected.**

Neither correction changes any conclusion G20-1 drew. Its §5 deliberately did not
rest on the counter alone, and that caution was justified.

## 4. **The meaning of `invocations` is now stated, and it is not what records have assumed**

**Read every record in this directory that quotes an `invocations` number in
light of this paragraph.** No number changes; what a number *licenses* does.

* `invocations = N` means **at least N** Java-level calls, and exactly N
  registry-resolved dispatches. It is a **floor**.
* `invocations = 0` does **not** mean the body is dead. It means no counted
  dispatch reached it. A body reached only through the intrinsic table or a JIT
  direct-call helper reads 0 or near-0 forever.
* The safe reading remains the one a lane was already given: treat it as *"did
  this ever run, through a counted path"*. That reading was correct and is now
  justified rather than merely cautious.
* **`owns_slot` is unaffected and remains the authoritative answer to "which
  body would run".** `HANDOFF-20260814` §4's recommendation of this dump stands;
  it is the `invocations` column, not the dump, that needed the caveat.
* For an exact `invocations` column, run with **`--nojit` and
  `CRATONVM_DISABLE_INTRINSICS=1`**.

One consequence reaches the CI gate. `invocations_of_kind(SyntheticStub) == 0`
inherits the same blind spots, so it is sound in one direction only: non-zero
proves a stub ran; zero does not prove none did. It has not been wrong yet
because neither bypass family currently serves a `SyntheticStub` — both tables
are `java.base` intrinsics and collection fast paths, all `Bridge` or
`Intrinsic`. That is a fact about today's contents, not a property of the gate.
The doc on `invocations_of_kind` now says so.

## 5. The cost of a true count — MEASURED, and it is the reason not to add one

The brief's constraint was "a true count must not cost measurable throughput".
It does.

`scratchpad/bench_counter.rs`, `rustc -O`, the exact shape of `record_invocation`
(bounds-checked index into a 12,011-entry `Vec<AtomicU64>` — this branch
registers 12,011 natives — plus a relaxed `fetch_add`), 12 interleaved rounds
with the arm order flipped on odd rounds, medians and full spreads, 20,000,000
iterations per arm:

| slot pattern | counted | uncounted (index + `black_box`) | delta |
|---|---:|---:|---:|
| one hot slot | 10.465 ns [9.250 … 11.599] | 1.250 ns [1.197 … 1.752] | **+9.2 ns** |
| 64-slot spread | 9.681 ns [9.157 … 15.314] | 1.246 ns [1.209 … 1.535] | **+8.4 ns** |

Read against G20-1 §5's denominators:

* against the **~141 ns Rust native-call boundary**, +9.2 ns is ~6%. Affordable,
  and it is why the counter sits on the generic dispatch path today.
* against a **thin direct-call helper**, whose entire reason to exist is to be
  cheaper than that boundary, +9.2 ns is plausibly the whole margin. Paying it
  would partly undo `perf/halfgap-20260717`.

Hence the design taken: **do not add a hot-path counter. Make the field admit
what it does not know.** A per-slot bit, set once and cold at bind time, costs
nothing per call and turns a number that lies into a number that says "at
least".

(Caveat on the 9.2 ns: this host was running several agents throughout, and a
`lock xadd` is nearer 5 ns on an idle machine. The number is an upper bound on
this host. The *ratio* to the uncounted arm — 8x — is the load-independent part,
and it is the part the argument uses.)

## 6. What changed — PREDICTED (written, compiled clean by the orchestrator, not run)

Two files, both owned by this lane.

### `native-api/src/registry.rs`

* **`slot_invocations_incomplete: Vec<AtomicBool>`**, index-parallel with
  `slots`, grown in the same single arm as `slot_invocations`.
* **`mark_invocations_incomplete(&self, id)`** — a bypassing path declares
  itself once, cold, at bind time. One relaxed store. Sticky across
  re-registration by design: a call site already bound to the previous callback
  keeps calling it, so clearing the bit would manufacture a false claim of
  completeness. Its doc carries the list of sites that owe the call.
* **`invocations_complete(&self, id) -> Option<bool>`** and
  **`slots_with_incomplete_invocations() -> usize`** (cold, one relaxed load per
  slot, same justification as `invocations_of_kind`).
* **`NativeCensusEntry::invocations_complete: bool`**, read through the same
  `owner_slot` reverse index as the count itself. A superseded row reports
  `invocations: 0, invocations_complete: true` — a floor of zero on a row that
  can never be dispatched is a total, and inheriting doubt there would invent
  one.
* Doc contracts rewritten on `record_invocation`, `invocations_of_id`,
  `invocations_of_kind` and `NativeCensusEntry::invocations`, each carrying the
  measured numbers and the exact-census recipe rather than a pointer to this
  file.

Two tests: `invocations_are_complete_until_a_bypassing_path_says_otherwise`
(default, per-slot isolation, idempotence, the foreign-handle no-op, and that
marking does not disturb the tally) and
`the_incomplete_flag_survives_re_registration_and_reaches_the_census`.

**Nothing sets the bit yet.** Every row still reports
`invocations_complete: true`, and the JSON writer does not emit the field. Both
are nominations (§8). The API is the landing point; §4 is the load-bearing part
of this lane.

### `vm-cli/src/main.rs`

* **`SILENTLY_IGNORED_IF_MISPLACED`** — 13 flags whose misplacement produces
  silence rather than an error, plus **`misplaced_launcher_flags()`** (pure,
  order-preserving, deduplicating, handles `--flag=value`) and
  **`warn_about_misplaced_launcher_flags()`**, called from `main()` on
  `early_argv` before anything else runs.
* **`absolute_dump_path()`** — every dump message, **success as well as
  failure**, now prints the absolute path actually written. This is the fix for
  the POSIX-path complaint: on Windows `/tmp/reg.json` resolves against the
  current drive to `C:\tmp\reg.json`, the write *succeeds*, and the caller looks
  in the wrong place. The success case is the one that misleads.
* **`describe_dump_failure()`** — names the attempted absolute path and whether
  its parent directory exists, separating "no such directory" from "the
  directory is there and the write still failed". The OS error alone does not:
  on this host `os error 3`'s text is not even English.
* **`census_invocations_caveat()`** — the success line for
  `--dump-native-registry` now carries the lower-bound warning **next to the
  number**, and switches to "exact count in this configuration" when `--nojit`
  and `CRATONVM_DISABLE_INTRINSICS=1` are both set. A caveat that lives only in
  a design doc is a caveat that gets quoted around.
* **`schema 3` → `schema 4`.** The confirmation line said 3 while the writer
  emits `"schema_version": 4` and `--help` documents 4. Verified in a real dump
  file. A bad way for the instrument whose job is to be believed to introduce
  itself.
* `--help` for `--dump-native-registry` now states the lower-bound contract, the
  measured numbers, the exact-census recipe, and that `owns_slot` is unaffected.

Eleven tests, in the existing `#[cfg(test)]` module, alongside the existing
`insert_program_args_separator` block: detection after a bare main class and
after `-jar`, the no-false-positive case (`--dump-native-registry-v2`,
`--jdk-only-reporter`, `-nojit`), correctly-placed flags, dedup and argv order,
the explicit-`--` case, no-selector, and the four dump-path diagnostics.

## 7. Is the misplaced-flag family loud now? — PREDICTED

The "before" is MEASURED and reproduces exactly as reported:

```
cratonvm --java-home $JH -cp probe PutProbe 100 -1 --dump-native-registry late.json
  -> exit 0, no file, no diagnostic
```

The half of defect 2 concerning **unwritable** paths was **already loud** on this
binary — `[cratonvm] warning: could not write native registry JSON to <path>:
<os error>` — and the change only sharpens it. The **misplaced** half was
entirely silent and now emits one line per flag naming the flag, the
consequence, and the fix, plus a summary line with the suppression switch.

The family, not the one flag: `--dump-native-registry`, `--jdk-only-report`,
`--dump-class-origins`, `--dump-missing-natives{,-grouped}`,
`--dump-phase-report`, `--jdk-only`, `--explain-jdk-only`, `--trace-jdk-only`,
`--XX:AuditMissingNatives`, `--nojit`, `--stack-dump-on-timeout`,
`--stack-sample-ms`.

**Warning, never an error, and the argv is never rewritten.** A Java program is
entitled to an argument spelled `--jdk-only-report`; silently hoisting it out of
the program's own argv would be a worse bug than the one being reported.
`CRATONVM_NO_MISPLACED_FLAG_WARNING=1` silences it. `--nojit` and `--jdk-only`
are on the list for the sharper reason: a discarded one does not produce an
*empty* result, it produces a *confidently wrong* one — G20-1 §3 is a whole table
of paired JIT/`--nojit` arms.

Note that `/tmp` was **not** the failure the brief described. Git Bash rewrites
`/tmp/x.json` to `C:/Users/.../Temp/x.json` before the VM ever sees it, and the
dump lands where bash expects. The real hazard is the absolute-path one in §6,
which is why the success line now prints the resolved path.

## 8. NOMINATIONS — everything outside this lane's two files

Ranked by (evidence recovered) / (risk). N1 is two lines and closes the JIT half
of the defect outright.

### N1 — count the `HashMap` direct helpers, or mark them
**File:** `vm/src/jit/helpers.rs`, `jit_hashmap_put_direct` and
`jit_hashmap_get_direct`. **Evidence:** §2, mechanism 2 — 1,873 against 100,001.
**Two options, and the cheap one is probably right.** (a) Wrap them the way
`call_integer_native_raw` already wraps its siblings, at +9.2 ns/call (§5) on a
helper built to save ~141 ns — measure before landing. (b) Resolve the
`NativeMethodId` once at bind time in `jit::try_compile`'s `direct_native_helper`
block and call `registry.mark_invocations_incomplete(id)` there — free per call,
and the census then says "at least 1,873" instead of "1,873". **(b) unless a
measurement says (a) is free.**

### N2 — mark the intrinsic table's slots
**File:** `vm/src/runtime/interpreter/`, wherever `populate_invoke_cache`
installs `CachedInvokeTarget::Intrinsic`. **Evidence:** §2, mechanism 1 — the
`Math.abs` 1 → 100,000 causal test. Same `mark_invocations_incomplete` call, once
per cache fill, cold. **This is the bigger of the two**, because it is
arm-independent and therefore invisible to the `--nojit` cross-check every lane
reaches for first.

### N3 — emit `invocations_complete` in the census JSON
**File:** `vm/src/vm/vm_init.rs`, `dump_native_census_json` (the
`"schema_version": 4` writer). One field per row from
`NativeCensusEntry::invocations_complete`, plus
`slots_with_incomplete_invocations()` in the header block. **Schema bump to 5**,
and `difftest/src/census.rs` reads this file. Worth nothing until N1 or N2 lands
— all rows would read `true`.

### N4 — the signature-polymorphic census gap is still open
**File:** `vm/src/vm/vm_exec.rs` ~25460 already documents it: the three `find`
loops for `MethodHandle` / `VarHandle` dispatch without counting, so every
signature-polymorphic native reads `invocations: 0` forever. The marker says W8-1
was misled by exactly this. It is a **third** instance of the same species as §1
and belongs in the same sweep. Its own note explains why it is not a one-liner.

### N5 — correct G20-1 §8 in place
**File:** `docs/known-issues/jdk-only/G20-1-...md`. §3 above supplies the three
amendments. G20-1 is heavily cited; leaving "under-reports in the `--nojit` arm"
standing will send the next lane looking for a mechanism that is not there.

### N6 — a `--census-exact` convenience flag
**File:** `vm-cli/src/main.rs` (this lane's, but out of scope and unmeasured):
sugar for `--nojit` + `CRATONVM_DISABLE_INTRINSICS=1` + `--dump-native-registry`.
Nominated rather than written because nothing measured says the two-part recipe
is a stumbling block, and this lane's §5 is a demonstration of what happens when
a claim is asserted rather than counted.

## 9. What this lane did NOT do

* **Did not build, run, or test its own edits.** Every claim in §6 and §7 about
  the *new* behaviour is **PREDICTED**. The orchestrator reports
  `cargo check --workspace --tests` clean; that is compilation, not behaviour.
  Nobody has seen the misplaced-flag warning print, and nobody has seen
  `invocations_complete` come out of `census()`.
* **Did not fix defect 1.** Nothing sets the incomplete bit. The instrument now
  *can* admit its blind spot and still *does not*. The diagnosis and the API are
  the deliverable; N1 and N2 are the fix.
* **Did not touch defect 3** (`--jdk-only-report` as an unused census). Defects 1
  and 2 took the budget, exactly as the brief ordered. The census record's own
  finding — that the census over-reports while the probe under-reports, and only
  their intersection is sound — is untouched and still the right frame.
* **Did not measure `record_invocation`'s cost in the real VM.** §5 is a
  standalone `rustc -O` microbenchmark of the same code shape, not an A/B of two
  CratonVM binaries, because this lane could not build one. It is the right
  order of magnitude and the wrong kind of evidence for a landing decision;
  N1(a) needs a real A/B.
* **Did not explain `String.charAt`'s absence from the registry.** Confirmed
  absent — not zero-valued, *absent* — and not chased further. Something
  registers it conditionally or not at all in this build.
* **Did not explain `Object.hashCode = 97,001`** in the JIT-on/intrinsics-on
  arm, where the interpreter arm reads 1. Some mixture of counted and uncounted
  routes; not isolated.
* **Did not audit whether any `SyntheticStub` is reachable through either bypass
  family.** §4's claim that none is today rests on reading what those two tables
  contain, not on an exhaustive check. If one ever is, the CI gate goes quiet
  rather than red, and that is the failure mode worth a dedicated probe.
* **Did not change `owns_slot`, `kind`, `registered_by`, `overwrote`,
  `kind_stated`, `kind_chosen`, or any count in the `counts` block.** No record
  quoting those needs re-reading.

## 10. Reproduce

```bash
export JAVA_HOME="C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot"
CV="C:/craton/target-rel/release/cratonvm.exe"      # 783685c34
SP=scratchpad/probe

# Mechanism 1, the causal test. Compare columns 1 and 2.
for D in "" 1; do
  CRATONVM_DISABLE_INTRINSICS=$D CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 "$CV" \
    --java-home "$JAVA_HOME" --nojit \
    --dump-native-registry "$SP/i$D.json" -cp "$SP" IntrProbe 100000
done
# Math.abs: 1 with intrinsics on, 100000 with them off.

# Mechanism 2. Compare arms, then vary n: the JIT arm does not move.
for J in "" "--nojit"; do
  CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 "$CV" --java-home "$JAVA_HOME" $J \
    --dump-native-registry "$SP/p$J.json" -cp "$SP" PutProbe 50000 -1
done
# HashMap.put: 2000 (JIT) against 50000 (--nojit).

# The counter's cost.
rustc -O --edition 2021 -o bench_counter.exe scratchpad/bench_counter.rs && ./bench_counter.exe
```

**The flag must precede the main class.** That is what §7 is about, and until N1
of the CLI change is in a binary you can run, it is still silent.
