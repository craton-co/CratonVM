# JIT LICM / speculative pre-header bypassed by a branch into the loop header — FIXED 2026-07-27

**Status: ✅ FIXED** (`jit/src/x64.rs`, `find_bypassable_loop_headers`).
General VM correctness bug — not app-specific. Found while closing
`docs/internal/java-io-writer-write-char-array-jit-miscompile-20260726.md`
and lifting the `org/glassfish/jaxb/` JIT ban.

## Symptom

Two faces, both from the same defect:

1. **`OutOfMemoryError: Java heap space (anewarray component 6 length
   1677721600)`** raised from inside a JIT-compiled
   `org.xml.sax.helpers.AttributesImpl.ensureCapacity` — the HIB-LONGTAIL.2
   signature. `1677721600 == 25 * 2^26` and `838860800 == 25 * 2^25`: the
   `while (max < n * 5) max *= 2;` growth loop doubled `max` from its initial
   25 against a garbage bound. Roughly **1 run in 3** (6/20, then 11/20 in a
   second sample of a sharper probe).
2. **A `while` loop silently not executing at all** — `shapeB` in the
   regression witness returns 25 instead of 400 on **199266 of 200000** calls
   once compiled, because the same garbage bound happened to be small.

## Root cause

Every speculative pre-header this backend emits — aaload / integer / FP LICM
hoists, speculative-BCE range guards, SIMD batch preheaders — is emitted
**inline at the loop-header PC**, and `pc_to_native[header]` is then set to the
position **after** it, so the in-loop back edge does not re-run it. (The
`osr_entry_native` field exists precisely because an OSR entry must land
*before* the pre-header; see its doc comment in `jit/src/x64.rs`.)

That contract silently assumes the only way into the loop is the linear
fall-through, which runs the pre-header first. It does not hold when the loop
header is **also the target of a forward branch from before the loop**:

```
 0: iload_1
 1: ifeq 10
 4: bipush 25
 6: istore_2
 7: goto 13          <-- enters the header directly, skipping the pre-header
10: bipush 30
12: istore_2
13: iload_2          <-- loop header: pre-header emitted here, pc_to_native[13]
14: iload_0              points past it
15: iconst_5
16: imul
17: if_icmpge 27
20: iload_2; 21: iconst_2; 22: imul; 23: istore_2
24: goto 13         <-- back edge (correctly skips the pre-header)
27: iload_2; 28: ireturn
```

That is `org.xml.sax.helpers.AttributesImpl.ensureCapacity` verbatim (JDK 25,
`javap -p -c`), and `LicmEntryProbe.shapeB`. arith-LICM hoists the
loop-invariant `n * 5` into a frame slot; the disassembly of the compiled
`ensureCapacity` shows the hoist emitted only on the fall-through path —

```
20f: mov rax,0x19          ; max = 25   (data == null branch)
216: mov [rbp-0x50],rax
21a: mov r12,rax
21d: jmp 0x37d             ; -> loop header, PAST the hoist below
...
356: mov rax,r13           ; n
35d: mov rax,0x5
36f: imul eax,ecx          ; n * 5
375: mov [rbp-0x38],rax
379: mov [rbp-0x30],rax    ; <-- the hoisted cache slot, written ONLY here
37d: mov rax,[rbp-0x30]    ; <-- loop header reads it
385: mov rcx,rax
388: cmp r12d,ecx
38b: jge 0x4b0
```

— so on the `goto` edge `[rbp-0x30]` is never written and the loop compares
`max` against whatever the previous call left at that frame offset. A small
leftover exits the loop immediately (face 2); a large positive leftover doubles
`max` up to ~1.6e9 and the following `new String[max]` dies (face 1). Both are
silent: no exception at the miscompile itself, no deopt.

The same placement contract also covers the **speculative-BCE range guards**,
whose elisions are only sound if the guard ran — a bypassed guard leaves
`bounds_safe_pcs` elisions in place with no check, i.e. a potential
out-of-bounds heap access. No such case was observed in the wild, but it is the
same edge and is closed by the same fix.

Why the common loop shapes are unaffected — and why the fix costs nothing
there: javac (JDK 25) emits `for`/`while` with the condition **at the top** and
a single `goto` back edge at the bottom. `LicmEntryProbe.shapeA`, a plain
counted `for` with an invariant `base*3+11`, compiles to

```
 2: iconst_0; 3: istore_3
 4: iload_3; 5: iload_1; 6: if_icmpge 28   <-- header, entered by fall-through
 9: … iload_0; iconst_3; imul; bipush 11; iadd …   <-- the hoisted run
22: iinc 3, 1
25: goto 4                                  <-- the only branch to the header
```

so the header's only non-back-edge predecessor is the fall-through that runs
the pre-header. It is *not* reported bypassable and keeps its hoist. shapeA is
correct before and after the fix — 0/200000 wrong in both.

## Fix

`find_bypassable_loop_headers(code, code_len, loops)` in `jit/src/x64.rs`
decodes every explicit branch edge in the method (conditional branches, `goto`,
`goto_w`, `tableswitch`, `lookupswitch` — same decoding as
`compute_branch_targets`, but keeping the *source* PC) and reports every loop
header for which some edge with a source outside `[header, loop_end)` targets a
PC inside `[header, loop_end)`. Methods containing `jsr`/`ret` are fail-closed
(every header reported), because `ret`'s successor is not statically known.

`compile_with_param_slots` then drops, for those headers only:

* `hoist_info` (aaload LICM),
* `arith_hoist_info` (integer LICM),
* `fp_hoist_info` (FP LICM),
* `speculative_bce_guards` — and, mirroring the existing per-bci de-spec drop,
  removes each dropped guard's `covered_pcs` from `bounds_safe_pcs` so those
  accesses get their per-element checks back,
* `simd_loops` / `simd_fp_loops` / `simd_element_wise_loops`.

**Trade-off, deliberate.** The alternative — routing external entries to a
second entry point before the pre-header (the `osr_entry_native` position) —
needs the *source* PC at all 21 `forward_patches` sites plus every immediate
backward-branch resolution, in a 42k-line emitter. This fix only ever *removes*
an optimisation, and only for a loop shape that is uncommon in javac output, so
it was preferred over the re-routing. If a future perf pass wants the
optimisation back for these loops, the correct shape is a second entry point,
not a relaxation of this predicate.

**Measured cost.** `CRATONVM_DBG_JIT_GEN=1` counts the methods that actually
receive an arith-LICM hoist, before vs after:

| Workload | before | after |
|---|---|---|
| `LicmEntryProbe` (both shapes) | 2 | 1 — only `shapeB`, the bypassable one, is dropped |
| `JaxbQNameProbe` (real JAXB marshal/unmarshal round trip, 300 iterations) | 0 | 0 |
| `BinTreesClassic d=14` | 0 | 0 |

i.e. on the two real workloads arith-LICM does not fire at all, so the gate
costs nothing there. A wall-clock A/B on `BinTreesClassic d=16` was attempted
but the build host was at load average 40-100 from concurrent sessions and the
run-to-run spread within a single binary (1355-3212 ms) swamped any difference —
no usable signal, and none is claimed.

**~~Known remaining limitation~~ — CLOSED 2026-07-27 (follow-up, see below):**
exception-handler edges were invisible to the JIT here
(`compile_with_param_slots` was not handed the exception table), so a handler
landing inside a hoisted loop body without passing the header would still
bypass its pre-header.

## Verification

Regression witnesses (`docs/internal/repros/jit-licm-preheader-bypass-20260727/`):

* `LicmEntryProbe.java` — minimal, deterministic, pure-user-bytecode.
  Before: `badB=199266/200000`. After: `badA=0 badB=0`.
* `AttrsCorruptProbe.java` — pure-JDK `org.xml.sax.helpers.AttributesImpl`,
  re-reads `getLength()` after every `addAttribute` (which proves the corruption
  is a *read* of the hoisted bound, not a corrupted `length` field: the OOM
  always fires before any `LEN-CORRUPT` line).
  Before: **11 failures / 20 runs**. After: **0 / 25**, then 0 / 10 in the final
  sweep.
* `docs/known-issues/repros/jitban-remaining-20260726/AttributesImplGrowthProbe.java`
  (the original HIB-LONGTAIL.2 witness) — before: 6 fail / 20 runs.
  After: 0 / 6.

Unit tests in `jit/src/x64.rs`:
`single_entry_loop_header_is_not_bypassable`,
`forward_goto_into_loop_header_is_bypassable` (the latter also asserts
`find_arith_loop_hoists` *does* match the run, so the test would still be
meaningful if the matcher changed).

Env bisection that pinned it before the disassembly (no rebuild needed):

| Config | AttrsCorruptProbe 20000, 20 runs |
|---|---|
| baseline | FAIL=11 OK=7 (2 timeouts under host load) |
| `CRATONVM_JIT_BISECT_SKIP=org/xml/sax/helpers/AttributesImpl.ensureCapacity` | FAIL=0 OK=20 |
| `CRATONVM_JIT_BISECT_ONLY=org/xml/sax/helpers/AttributesImpl` | FAIL=7 OK=12 — defect self-contained in that class |
| `CRATONVM_JIT_GETFIELD_HELPER=1` | FAIL=9 OK=9 — not the compact-getfield path |
| **`CRATONVM_DISABLE_ARITH_LICM=1`** | **FAIL=0 OK=20** |

Final sweep against the shipped binary (LICM fix **and** the
`org/glassfish/jaxb/` ban deleted from `vm/src/jit/skip_list.rs`):

```
VERIFY[jaxb-qname-4000]           OK=6  BAD=0
VERIFY[attributesimpl-growth-20k] OK=6  BAD=0
VERIFY[attrs-corrupt-20k]         OK=10 BAD=0
VERIFY[licm-entry-200k]           OK=3  BAD=0
```

Suites: `regression-suite/run.sh` 13/13; `cargo test --release -p cratonvm-jit`
1021 lib + 191 integration tests, 0 failures;
`cargo test --release -p cratonvm-vm --lib skip_list` 68/68.

## Fallout this closes

* **HIB-LONGTAIL.2** (`AttributesImpl.ensureCapacity`) — its 2026-07-26 removal
  from `vm/src/jit/skip_list.rs` was recorded as "no longer reproduces", but the
  bug was ~30%-per-run flaky and the re-verification did not run enough
  repetitions. It did still reproduce on 2026-07-27's dev; it is now genuinely
  fixed rather than under-sampled. **Lesson: a flaky ban must be re-verified
  with a run count, not a single green run.**
* The `org/glassfish/jaxb/` ban's 4000-iteration probe, which kept dying on this
  bug inside `SAXOutput.attribute` → `AttributesImpl.addAttribute` before the
  JAXB-specific code path was reached. See
  `docs/internal/jaxb-jit-ban-removed-20260727.md`.

## Other backends — checked, not affected

* **aarch64** (`jit/src/aarch64_backend.rs`, `jit/src/aarch64.rs`): no LICM,
  speculative-BCE or SIMD pre-header machinery at all (grep for
  `hoist`/`LICM`/`preheader` finds only an unrelated comment), so there is no
  equivalent placement contract to break.
* **IR backend** (`jit/src/ir_optimize.rs::licm`): CFG-based, and
  `loop_headers` only yields a loop when `entry_preds.len() == 1` — a header
  with a second entry edge is skipped outright, and `licm` additionally bails
  when the classified pre-header is `NO_NODE` or lies inside the body. Sound by
  construction; no change needed.

---

## Follow-up 2026-07-27: the exception-handler edge is closed (pre-emptively)

The limitation noted above is now handled. `find_bypassable_loop_headers` takes
the method's exception table as `(start_pc, end_pc, handler_pc)` triples, staged
by `try_compile` through a one-shot thread-local (`set_pending_exception_ranges`,
the same idiom as `set_pending_verified_max_stack`) and taken at backend entry so
a bailed compile cannot leak it into the next method on that worker.

**The predicate is the branch-edge rule with the throwing site standing in for
the branch source** — an entry into the loop body whose source is outside the
loop:

    handler_pc ∈ [header, loop_end)  AND  (start_pc < header OR end_pc > loop_end)

The second clause matters. A protected range lying *wholly inside* the loop is
sound and keeps its hoist: the throw can only have happened after the header was
entered, so the pre-header had already run. That is exactly what javac emits for
`while (…) { try { … } catch { … } }`, the overwhelmingly common try-in-a-loop —
a blunter "any handler inside the loop" rule would have dropped LICM for all of
those for no correctness gain. Only a range reaching before the header (or past
the loop) can deliver control into the body from code the pre-header never
covered.

### Honest status: this is hardening, not an observed bug fix

**No runtime witness exists today, and cannot.** Methods with a non-empty
exception table are not admitted to this backend at all, so
`find_bypassable_loop_headers` never sees a handler in the first place.
Measured with `CRATONVM_DBG_DUMP_JIT=LIST` on dev at `d134f7104`:

| probe | exception table | compiled? |
|---|---|---|
| `LicmEntryProbe.shapeA/shapeB` | none | **yes** — both listed |
| `TryCatchHot.f` (try/catch in a hot loop, 200 000 calls) | yes | **no** |
| `HandlerLoopProbe.shape` (the witness below, 400 000 calls) | yes | **no** |

So the change is provably inert on the current tree: with an empty table the new
loop body never executes and codegen is byte-identical. It is landed because the
admission gate has already been relaxed once (RBC.6 admitted explicit `athrow`),
`precise_exception_frames` exists to relax it further, and the day handler-
bearing methods reach the backend this hazard is live and silent — a wrong loop
bound or, worse, a speculative-BCE guard elided but never run.

### Dormant witness, ready for that day

`docs/known-issues/repros/jit-licm-handler-edge/GenHandlerLoop.java`
emits `HandlerLoopProbe.class` via ASM — a shape javac cannot express: the
handler PC sits inside the loop body while its protected range lies entirely
before the loop header, and the normal path **falls through** into the header so
there is no `goto` edge for the existing rule to catch. That isolates the
exception edge as the only external entry:

```
 0: iconst_1; 1: iload_2; 2: idiv; 3: pop     <-- protected [0,7), implicit throw
 4: bipush 25; 6: istore_3                        (explicit athrow is not admitted)
 7: iload_3; 8: iload_1; 9: iconst_5; 10: imul   <-- header 7; n*5 is the hoist
11: if_icmpge 31
14: goto 24
17: pop; 18: bipush 25; 20: istore_3; 21: goto 7  <-- handler 17, INSIDE [7,31)
24: iload_3; 25: iconst_2; 26: imul; 27: istore_3
28: goto 7                                        <-- back edge
31: iload_3; 32: ireturn
```

`HandlerLoopDriver.java` alternates the two entries 200 000 times; both set
`max = 25`, so both must return 50. Real HotSpot: `badNormal=0 badHandler=0`.
CratonVM today: also clean — because `shape` is never compiled. When the
exception-table gate is relaxed, run this driver first; it is the direct witness
for this edge.

Unit test: `handler_reachable_from_outside_a_hoisted_loop_is_bypassable` in
`jit/src/x64.rs` covers all four cases on hand-written bytecode — no handler,
range wholly inside (must KEEP the hoist), range starting before the header, and
handler outside the loop.

## Re-confirmed on the Hibernate ORM harness (2026-07-27, later the same day)

The same signature was re-reported from
`/data/data/apps/hibernate-orm-harness/`, aborting
`org.hibernate.orm.test.hql.{ASTParserLoadingTest, BulkManipulationTest,
ScrollableCollectionFetchingTest, TreatKeywordTest}` "right after JAXB
ContextFactory initialization during SessionFactory bootstrap". It was the same
bug: the binaries that produced it were built from a `dev` snapshot that
predates this fix.

| | |
|---|---|
| that A/B worktree branched from `dev` at | `c042e794f`, 07:57 UTC |
| this fix merged into `dev` at | `c11305865`, 11:29 UTC |
| the aborting runs executed | 17:48-18:47 UTC, on the 07:57 binaries |

`git merge-base --is-ancestor 613b10f4c c042e794f` is false, so *neither arm* of
that A/B had the fix — which is also why the abort reproduced with the JIT ban
list in its default state and with `org/antlr/v4/runtime/` unbanned. The ban was
never the variable.

### `component 6` is a ClassId, not a `newarray` atype — read it correctly

The report read `anewarray component 6 length 1677721600` as *T_FLOAT*, i.e. a
6.7 GB `float[]`, and looked for a corrupted length word. That is a misreading
the message format invites, and it sends the investigation to the wrong place:

* the field is `component_class_id_raw` — a raw `ClassId` — see
  `vm/src/jit/helpers.rs::jit_anewarray_object`;
* that helper is reference-arrays-only: it hardcodes
  `ArrayElementType::Reference`. Primitive arrays go through `jit_newarray`,
  a different helper with a different message;
* `ClassId 6` here is `java/lang/String`. `AttrsCorruptProbe`'s only
  `anewarray` is `String[]` and it emits this exact text;
* the length is already narrowed and sign-extended (`length as i32 as i64`)
  precisely to defend against stale-upper-half slot reads, and `1677721600`
  is a positive `i32` anyway.

So it is `new String[1677721600]`, and the constant is not corruption but
arithmetic: **`25 * 2^26`**, the `while (max < n * 5) max *= 2;` growth loop
doubling from its initial 25 against the un-written hoist slot.

### Why JAXB is where it surfaces

JAXB 4's `SAXOutput` does not use the JDK's `org.xml.sax.helpers.AttributesImpl`
— it uses its own `org.glassfish.jaxb.runtime.util.AttributesImpl`
(jaxb-runtime-4.0.9). `javap -p -c` on the two shows `ensureCapacity` is a
verbatim fork: same `goto 44` into the loop header at 44, same `iload_2;
iconst_2; imul` doubling at 51-55, same `anewarray java/lang/String` at 59. Both
copies hit this defect identically, and `find_bypassable_loop_headers` covers
both because the predicate is bytecode-shape-based.

Note this matters for coverage: at the 07:57 base the `org/glassfish/jaxb/` ban
was still in `skip_list.rs` (4 occurrences), so the JAXB copy was not
JIT-eligible then and those aborts came from the JDK copy. Current `dev` has
that ban lifted *and* this fix, so the JAXB copy is JIT-eligible for the first
time — hence the re-verification below rather than an assertion from ancestry.

### Verification, binary built from `dev` @ `c1ea5ac6d`

Strictly serial, one VM at a time, host load average 14-26.

| probe / class | runs | OOM |
|---|---:|---:|
| `AttrsCorruptProbe 20000 60` (11/20 pre-fix) | 25 | **0** |
| `AttributesImplGrowthProbe 20000` (6/20 pre-fix) | 15 | **0** |
| `org.hibernate.orm.test.hql.ASTParserLoadingTest` | 4 | **0** |
| `org.hibernate.orm.test.hql.BulkManipulationTest` | 3 | **0** |

`BulkManipulationTest` was 50 ok / 0 failed / 1 skipped on all 3.
`ASTParserLoadingTest` was 106/106 on 3 of 4; the 4th reported `failed=1`, but
that run's `@@FAIL` line was not captured, so the failing test is **not
identified here** — it is not an OOM, and the same class produced a
`TimeoutException: testJpaTypeOperator ... timed out after 120 seconds` on both
arms of the earlier A/B (the harness sets
`junit.jupiter.execution.timeout.default=120s`), which is the likely but
unconfirmed candidate.
