# `InPredicateTest` — 100k-element criteria `IN` predicate timed out under JIT — RETIRED 2026-08-04

| | |
|---|---|
| **Status** | ✅ **RETIRED 2026-08-04.** Root-caused and fixed: the callee compile gate scanned an inherited method's bytecode against the **subclass's** constant pool and permanently bail-listed it. `org.hibernate.orm.test.jpa.criteria.InPredicateTest` now PASSES under default JIT, and the JIT arm is the *fastest* of the three configurations measured. |
| **Area** | JIT callee admission (`try_jit_compile_callee_slow`), surfaced via `org.hibernate.orm.test.jpa.criteria.InPredicateTest` |
| **Old symptom** | `java.util.concurrent.TimeoutException: testInPredicate(...) timed out after 120 seconds`. Class wall time 155–510 s across three separate investigations (2026-07-07, 2026-07-08, 2026-08-04); `--nojit` passed at ~71–110 s. |
| **Fix** | `fix/hib-inpredicate-dispatch-jit-20260804` — one argument in `vm/src/runtime/interpreter/jit_bridge.rs`, plus a regression test and the diagnostics that made the bug visible. |

## What it actually was

`try_jit_compile_callee_slow` resolves a callee with `find_method_recursive`,
which returns the method **and** the class that declares it. It then scans that
method's bytecode for forced `Class` generic-metadata calls
(`jit_method_calls_forced_class_generic_metadata`) — a scan that resolves the
constant-pool indices embedded in the code.

Those indices only mean anything in the pool of the class that **declares** the
method. The gate passed `callee_class_id` — the **receiver's** class. For any
*inherited* method that is a different class with an unrelated pool, so the same
index came back as a Utf8, a Fieldref, or nothing at all. The scan's
conservative "shape I don't recognise ⇒ refuse" arm then fired and the method
was `mark_jit_bail_listed`ed **permanently**, for a constant it never
referenced.

```rust
// before — receiver's pool, declaring class's bytecode
jit_method_calls_forced_class_generic_metadata(&cm, callee_class_id, &code_attr.code, ...)
// after
jit_method_calls_forced_class_generic_metadata(&cm, declaring_id, &code_attr.code, ...)
```

The cost landed on exactly the code least able to absorb it: framework
hierarchies whose hot methods are **inherited accessors**. Hibernate's SQM tree
is precisely that shape. `CRATONVM_DBG=mic-prof` on the 20k-element repro:

```
mic_calls=2326211 hit_entry=0 hit_noentry=2228371 pub_probe_none=2228371
```

`hit_entry=0` — compiled code's monomorphic inline cache never once held a
callable entry. All 2.2M virtual dispatches out of compiled code took the
entryless helper path (`jit_invoke_virtual_mic`) instead of a direct call. The
per-(reason, callee) tally named the refusals:

```
258991 BAIL-LISTED org/hibernate/query/sqm/tree/domain/SqmTextValuedSimplePath.getReferencedPathSource
218991 BAIL-LISTED org/hibernate/metamodel/model/domain/internal/BasicSqmPathSource.getExpressible
217988 BAIL-LISTED org/hibernate/type/internal/NamedBasicTypeImpl.getExpressibleJavaType
178486 BAIL-LISTED org/hibernate/query/sqm/tree/domain/SqmTextValuedSimplePath.getResolvedModel
```

Every one of those is inherited. None of them declares a single
`Class.getTypeParameters`/`getGenericInterfaces`/`getGenericSuperclass` call.

That is why turning the JIT **on** made this workload *slower than the
interpreter*: `probes/JitCallEdgeProbe.java` prices the compiled-caller →
interpreted-callee edge at roughly 4x a fully-interpreted call and ~200x a
fully-compiled one, and this bug forced every hot call in the workload onto that
edge.

## Verification

`InPredicateTest`, solo, one class per process, arms interleaved, same host:

| binary | ms | result |
|---|---|---|
| pre-fix, JIT on | 320066 / 198315 | **FAIL** — `TimeoutException` @ 120 s |
| **post-fix, JIT on** | **87772 / 65055** | **PASS** (`ok=1 failed=0`), 2/2 |
| post-fix, `--nojit` | 152085 / 183117 | FAIL (see note) |

The `--nojit` rows are not a regression — the fix does not touch the interpreter,
and this host was running other sessions' builds throughout (the 2026-08-04
uncontended `--nojit` reading was 71 s). They are included because they make the
point precisely: under the same load only the **JIT** arm finishes inside the
120 s cap. The JIT went from a 1.6x loss to a 1.7x win on this test.

Phase-resolved repro (`apps/hib-suite-runner/InPredPhaseProbe.java`, n=20000,
3 interleaved rounds, per-thread **CPU** time because the host sat at 100%):

| arm | `.in()` | `createQuery` | total |
|---|---|---|---|
| pre-fix, JIT on | 14.4 s | 24.2 s | 38.8 s |
| pre-fix, `--nojit` | 6.2 s | 18.1 s | 24.5 s |
| **post-fix, JIT on** | **2.2 s** | **12.3 s** | **14.7 s** |
| post-fix, `--nojit` | 6.2 s | 19.5 s | 26.0 s |

`.in()` is 6.5x faster than the pre-fix JIT arm and 2.8x faster than the
interpreter. Post-fix `--nojit` is unchanged, as it must be.

Regression slice: 45 known-passing `jpa.criteria` / `query.hql` /
`query.criteria` classes, one class per process — 44 fully clean, 1 with two
dialect-conditional skips, **0 failures**, identical to the baseline binary.

Regression test: `vm/tests/jit_inherited_callee_constant_pool.rs`. It asserts
the inherited accessor is not bail-listed when resolved through the subclass,
**and** separately asserts the fixture's two constant pools genuinely disagree
at that index — so a future fixture edit that made them compatible fails loudly
instead of passing vacuously. Proven non-vacuous by reverting the one-argument
fix and re-running: `inherited_callee_is_not_bail_listed_through_subclass ...
FAILED`.

## Diagnostics added (the reason this took three attempts to find)

Two earlier investigations (2026-07-07, 2026-08-04) reached "dispatch-heavy
tier-up overhead, needs the lock-free tier-up work in
`project_wire_tiered_manager`" and stopped. That hypothesis was wrong — the
tier-up machinery had already been made lock-free on 2026-07-31 and
`CRATONVM_JIT=threshold=100000000` recovered nearly all the loss, which points
at *compiled code*, not at the counter path. What was missing was any way to see
inside compiled dispatch:

* **`--stack-sample-ms N`** — a time-weighted Java-frame profiler.
  `--stack-dump-on-timeout` latches one dump per nested `execute()`, so what it
  produces is a **call-count** trace wearing a profile's clothes: it reported
  100% of samples inside a three-bytecode `hashCode` that a micro-probe then
  priced at 7 µs of a 16 s phase. Sampling mode consumes the dump request
  instead of latching it, so a sampler thread re-arming it every N ms yields one
  dump per interval per running thread. Off by default; JIT-compiled frames
  never reach the dispatch loop, so pair it with `--nojit`.
* **`CRATONVM_DBG=callee-probe`** now tallies per `(reason, callee)` and dumps
  the histogram alongside the `mic-prof` report, carrying the recorded bail site
  into the key. Its previous first-25-lines sample was spent entirely on class
  loading and could never name the callees `pub_probe_none` implicated.

Probes: `probes/JitCallEdgeProbe.java` (prices the compiled→interpreted edge),
`probes/MapGetKeyedProbe.java` and `probes/NativeCallbackCostProbe.java` (the
controls that refuted the map-lookup and native-callback hypotheses),
`probes/ThreadCpuTimeProbe.java`, and
`apps/hib-suite-runner/InPredPhaseProbe.java`.

## Not claimed here

The absolute gap to HotSpot on this workload is untouched and large: warm
HotSpot does the same 100k-element `createQuery` in ~160 ms. That is the
ordinary interpreter/compiler maturity gap, not a defect this note tracks, and
`InPredicateTest` passing does not speak to it.

<details>
<summary>History (superseded)</summary>

1. **2026-07-05** — `NullPointerException: ... because "values" is null`, fixed
   2026-07-06 (`084c8ffb`).
2. **2026-07-06** — a `LinkedHashMap.removeEldestEntry` `NoSuchMethodError`,
   fixed 2026-07-08 in the LinkedHashMap native guard.
3. **2026-07-07** — first `TimeoutException` investigation. Confirmed real via
   three clean uncontended runs (330–510 s). Correctly **ruled out** two
   same-day-landed suspects: the precise-JIT-maps default-ON re-flip
   (`CRATONVM_NO_PRECISE_JIT_MAPS=1` was slightly *slower*) and the OSR
   allocation/call-region gate (`CRATONVM_DBG_OSR=1` showed 5 `enter` events and
   zero `REJECT`s). Landed on the dispatch-heavy tier-up hypothesis.
4. **2026-07-08** — retired on a single passing run. That was wrong twice over:
   one run is not the multi-run evidence the original investigation used, and
   the underlying defect was still there.
5. **2026-08-04 (reopen)** — reproduced the identical signature on `dev` tip
   `a43a74ded`, both in-suite and solo.
6. **2026-08-04 (this note)** — root-caused to the constant-pool mismatch above
   and fixed.

The 2026-07-07 A/B refutations of precise-JIT-maps and the OSR gate still stand;
they were correct, and re-confirmed here (`CRATONVM_JIT=osr=0` and
`CRATONVM_JIT=virtual-tierup=0` both left the regression intact, while
`threshold=100000000` removed it).

</details>

## Repro (historical)

```
cd apps/hib-suite-runner
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 <cv-binary> \
  --java-home "C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot" \
  --Xmx 1500m @common.args -Dcraton.batch=1 \
  CratonRunner org.hibernate.orm.test.jpa.criteria.InPredicateTest
```
