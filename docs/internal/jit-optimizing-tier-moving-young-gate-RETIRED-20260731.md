# The optimizing (C2/IR) tier and the moving-young gate — retired

**Status: ✅ RETIRED 2026-07-31.** Retired from
`docs/known-issues/jit-optimizing-tier-disabled-by-moving-young-default.md`;
that document's full text, including its own corrections, is preserved below
the fold because two of its headline claims were wrong in ways worth keeping.

The original defect — `try_compile_inner` admitting the optimizing tier only
when `!x64::moving_young_enabled()`, against a `DEFAULT_MOVING_YOUNG = true` —
is fixed and stays fixed. What kept this document open afterwards was a chain
of residuals, and this closes them.

## The short version

| claim | verdict |
|---|---|
| the moving-young gate switches the optimizing tier off by default | **was true, fixed** (`f78b72670`, scoped to `moving_young_relocates_compiled_frames`) |
| the same scoping was applied to the raw JIT-to-JIT direct-call gate, where it is unsound | **true**; re-gated, and the edge itself is fixed separately — see "Residual 1" |
| "the optimizing tier does run on default flags" | **true of admission, and it was misleading**: the tier was admitted and then produced ~30 bodies per workload, refused inside the BUILDER |
| the ASTParser 376→138 s and Oracle 322→93 s wins came from restoring C2 | **false**, and now positively disproved: at that commit the tier emitted nothing, and when it does emit it was *slower* — see "What C2 was actually worth" |

## What the tier was actually doing

Measured on `org.hibernate.orm.test.mapping.converted.converter.YearMonthConverterTest`
with `CRATONVM_JIT_FORCE_C2=1 CRATONVM_DBG_IR_COMPILES=1` (the per-stage refusal
reporting added by the relocation-contract work), on `origin/dev` at
`b2f7c83cc`:

```
244 admission decisions, 171 admitted to the optimizing pipeline
 30 IR bodies produced
```

Every one of the 141 losses was named, and not one of them was the gate:

| refusal | count | what it is |
|---|---:|---|
| `ir.rs:1864` | 66 | `getfield` of a non-int-category field — i.e. every reference field |
| opcode `0xb0` | 25 | `areturn` — any method that returns an object |
| opcode `0xb2` | 15 | `getstatic` |
| `ir.rs:2000` | 14 | `invokespecial` of a non-elidable `<init>` |
| `0xc6`/`0xc7` | 9 | `ifnull` / `ifnonnull` |
| `0x01` | 4 | `aconst_null` |
| `0x12`/`0x13` | 4 | `ldc` / `ldc_w` |
| `0xbe` | 1 | `arraylength` |

plus, one stage earlier in `ir_compatible`: `typecheck_ops` (checkcast /
instanceof) 100, `has_athrow` 38, `anewarray` 10, `invokedynamic` 2.

So "the optimizing tier is disabled" and "the optimizing tier cannot compile
ordinary Java" produced the same observable for months, and the document
attributed all of it to the gate.

## What this branch closed

### 1. Builder coverage — 30 bodies → 60 on the same class

`aconst_null`, `areturn`, `ifnull`/`ifnonnull`, `if_acmpeq`/`if_acmpne`, and
`getfield` of a reference field. The last is the large one: `jit_getfield`
already returns the raw pointer for a compact reference slot, so only the node
TYPE differed — `Op::Load(MemKind::Ref)` / `IrType::Ref`, which also makes the
result slot publishable in every safepoint map. `lower_inner` refuses a graph
that would need the layout-naive inline fallback for a reference load.

`Op::Cmp` now selects a 64-bit compare when either operand is a reference. It
emitted `CMP EAX, ECX`, which for `ifnull` calls a pointer with a zero low word
null, and for `if_acmpeq` calls two references 4 GiB apart equal. Neither
crashes; both branch wrongly. `ir_vs_singlepass` covers both with operands
chosen so a 32-bit compare gives the opposite answer.

### 2. A shadow-stack leak that corrupted the C heap

Widening coverage made `BinTreesClassic.itemCheck` IR-eligible, and the process
died with SIGSEGV **inside mimalloc's free list** — ASCII bytes in a block
header, on the background compiler thread, arbitrarily far from any JIT code.

`Op::Call` emitted the safepoint map, and with it `emit_shadow_push`, BEFORE
choosing the dispatch route; the self-recursive route then "withdrew the claim"
by clearing `pending_shadow`. That withdrew the map's promise but left the push
in the instruction stream — and the self-recursive route deliberately bypasses
`emit_call_return_check`, the one site that emits the matching reload. Every
execution of such a call site advanced the thread's shadow `top` and nothing
retracted it, so a recursive method walked `top` off the end of the shadow
stack and the next push wrote into whatever followed the mapping.

Fixed by deciding the route before emitting the map
(`emit_safepoint_map_publishing(.., publish = false)`), and pinned by an
invariant: `lower_inner` counts emitted pushes and reloads and discards the
body if they disagree. The failure mode is now a compile that falls back to
single-pass, not a corrupted process.

This bug predates the coverage work — any IR-compiled self-recursive method
with a live reference leaked — it was simply unreachable while the builder
refused every such method.

### 3. What C2 was actually worth, and the mechanism that made it negative

With `itemCheck` finally compiling, `BinTreesClassic 18` at `-Xmx512m`, five
interleaved reps, one binary:

| lane | ms | median |
|---|---|---|
| default (single-pass body) | 3614, 3635, 3640, 3762, 3662 | 3640 |
| `CRATONVM_JIT_FORCE_C2=1` | 6987, 7013, 6914, 6436, 6752 | 6914 |

**1.85x slower.** The control that isolates it: before the coverage work, the
same two lanes on `bt16` were 670 ms vs 705 ms **with zero IR bodies in both**
— so the cost is the IR body, not the forced compile path.

The mechanism is not subtle once named: `Op::Load` routed every field read
through the `jit_getfield` helper — a JIT-boundary note, an `is_object_address`
region walk and a 16-byte atomic cell read — while the single-pass backend
emits a guarded inline read. That backend measured the same trade at **4.7x on
bintrees-16** when the helper hardening first landed, which is why it grew
`guarded_inline_getfield_enabled`. The IR tier never got one.

It has one now (`ir_lower::emit_inline_compact_getfield`), mirroring the
single-pass shape: null → helper, unaligned → helper, outside the published
`JIT_REGION_BOUNDS` → helper, legacy (non-`GC_FLAG_COMPACT`) instance → helper,
otherwise a raw read at the resolved packed offset. Same five interleaved reps:

| lane | ms | median |
|---|---|---|
| default | 3585, 3608, 3703, 3892, 3673 | 3673 |
| `CRATONVM_JIT_FORCE_C2=1` | 3732, 3777, 3853, 3797, 3742 | 3777 |

**Parity** (+2.8%, ranges overlapping). Checksum `68332206` in every lane of
every measurement above, and `14985902` for `bt16`.

Read that as the honest answer to the question this document has been asking
since it was filed: **restoring the optimizing tier was never worth the
throughput it was assumed to be worth.** Until this branch it was a 1.85x
pessimization on the one workload that could exercise it, and it is now
approximately free. Anyone who wants it to be a WIN has a starting point — the
remaining refusal inventory below, and the fact that `Op::Store`, `getstatic`
and array access still take helpers where single-pass inlines them.

### 4. Verification

Default flags throughout — none of these needed `CRATONVM_NO_MOVING_YOUNG=1`,
which is the thing the original document was filed about.

| target | result |
|---|---|
| `cratonvm-jit --lib` | **1065 passed / 0 failed** |
| `cratonvm-jit tests/ir_vs_singlepass.rs` | **92 / 0** |
| `cratonvm-jit`, every other target | 0 failed |
| `cratonvm-gc --lib` | **873 / 0** |
| `cratonvm-vm --lib` | **2304 / 0** (111 ignored) |

`BinTreesClassic` returns `68332206` (d=18) and `14985902` (d=16) in every lane
of every measurement in this document — default, `CRATONVM_JIT_FORCE_C2=1`,
both bisect levers, and `CRATONVM_NO_MOVING_YOUNG=1`.

## Residuals, and where they now live

### Residual 1 — the raw JIT-to-JIT direct-call edge

Re-gated in `ea5b2df6e` after `BasicErrorControllerIntegrationTests` SIGSEGV'd
8/8 and 14/14 with the gate open. This was being fixed in parallel on
`fix/jit-raw-jit2jit-edge-20260731` ("three shadow-stack/mirror imbalances at
the raw JIT-to-JIT edge") — the same class of defect as §2 above, on the
single-pass side. Its acceptance gate is unchanged: that class, 14 consecutive
runs, default flags. **Not claimed closed here.**

### Residual 2 — the `type.temporal` Hibernate pair — CLOSED, and its premise does not hold

The claim was: those classes exceed the suite's 300 s cap on default flags and
pass under `CRATONVM_NO_MOVING_YOUNG=1`, therefore a moving-young cost survives
the two relocation-scoped gates. Two named candidates were shipped with a
bisect lever each — `CRATONVM_JIT_MY_SCRATCH_FLUSH=0` (the scratch-register
flush at every GC-capable safepoint) and `CRATONVM_JIT_MY_SELFCALL_PROOF=0`
(the stronger proof before a self-recursive call may elide its spill).

Measured the way the retired relocation-contract document says to measure this
— a deterministic, call-dense, entry-dense probe with interleaved lanes on one
binary, not one run of a 300-second suite class — `BinTreesClassic 18` at
`-Xmx512m`, five reps:

| lane | ms | median |
|---|---|---|
| default | 4388, 5244, 4281, 3708, 4194 | 4281 |
| `CRATONVM_JIT_MY_SCRATCH_FLUSH=0` | 4117, 4605, 4479, 4056, 3999 | 4117 |
| `CRATONVM_JIT_MY_SELFCALL_PROOF=0` | 4343, 4096, 4797, 4660, 3618 | 4343 |

**Neither lever moves it.** Ranges fully overlap the default in both
directions, so neither named candidate is the residual.

And the premise itself inverts on this probe. Six interleaved reps, taken as
the host quieted (load average 32 → 24):

| lane | ms |
|---|---|
| default | 4542, 4904, 4305, 4668, 4337, 4060 |
| `CRATONVM_NO_MOVING_YOUNG=1` | 6440, 6850, 6042, 5912, 6125, 5741 |

**Disabling the moving young generation costs ~1.37x here, 6/6, ranges
non-overlapping** — checksum `68332206` throughout. So "the class passes under
`CRATONVM_NO_MOVING_YOUNG=1`" cannot be read as "moving-young costs
throughput": that flag selects a different collector, and which one wins is a
property of the workload. On the one deterministic probe available it wins for
moving-young by a wide margin.

And the lane the residual is stated in **does not run at all any more**.
`CRATONVM_NO_MOVING_YOUNG=1` on `ZonedDateTimeTest` SIGSEGVs after ~3 s, inside
a live JIT code buffer, reading a frame slot holding zero — the reclaimed-root
signature. On a pristine `origin/dev` build, so not this branch's; the flag
also switches off the single-pass backend's shadow-stack root publication,
because `shadow_stack_maps_enabled()` is
`flags().jit.shadow_stack || moving_young_enabled()`. Adding
`CRATONVM_SHADOW_STACK=1` back removes the crash. Filed as
`docs/known-issues/jit-no-moving-young-opt-out-unpublishes-roots.md`.

Meanwhile both classes **complete on default flags**, every run, correct:

| class | lane | wall | result |
|---|---|---:|---|
| `ZonedDateTimeTest` | default | 447 s | `found=608 ok=404 failed=0 aborted=204` |
| `ZonedDateTimeTest` | default | 435 s | same |
| `ZonedDateTimeTest` | default | 391 s | same |
| `ZonedDateTimeTest` | `NO_MOVING_YOUNG=1` | 3 s / 2 s / 2 s | **SIGSEGV**, all three runs |
| `OffsetDateTimeTest` | default | 367 s | `found=488 ok=324 failed=0 aborted=164` |
| `OffsetDateTimeTest` | default | 308 s | same |
| `OffsetDateTimeTest` | `NO_MOVING_YOUNG=1` | 3 s / 1 s | **SIGSEGV**, both runs |

Over the suite's 300 s cap on a host carrying 30–50 load average from other
tenants, but neither a timeout nor a failure. **The residual as written —
"TIMEOUT >900 s by default, passes under `CRATONVM_NO_MOVING_YOUNG=1`" — is now
inverted in both halves.**

Two further reasons this residual was never evidence:

* `ZonedDateTimeTest` is **bimodal** with no VM change at all — roughly 300 s
  or past 900 s, documented in
  `docs/internal/jit-ir-relocation-map-contract.md` with the eight-run table
  that shows it. A single sample per lane cannot support any attribution, and
  that is exactly what the claim rested on.
* The host it would have to be re-measured on carries 30–140 load average from
  other tenants; a 300 s class routinely doubles under that (see
  `reference_azure_build_host`). An absolute-threshold question ("does it
  exceed the 300 s cap?") is not answerable there.

What remains true and worth keeping: if a moving-young throughput cost exists
outside the two gates, neither of the two candidates named for it is that
cost, and the levers stay in the tree (default-on, no behaviour change) for
whoever looks next.

### Residual 3 — the remaining optimizing-tier inventory

Not defects; named exclusions, with counts from the run above. In descending
order of population: `checkcast`/`instanceof` (100), `athrow` (38), `getstatic`
(18), `arraylength` (9), non-elidable `<init>` (5), `ldc` (4), array
load/store, `newarray`/`anewarray`, `multianewarray`, `invokedynamic`. Plus two
structural ones that no opcode count reveals:

* **`call_eligible`** in `try_compile_inner` disables invoke lowering for any
  method containing `new` or `anewarray`, so "allocates and calls" — most
  constructors and factories — cannot reach the tier at all. It is a leftover
  of the incremental activation programme (`6e1b15aab`), not a proven hazard.
* **C2 is almost never REQUESTED.** The tiered manager will recommend C2 at
  `c2_threshold` (20 000) invocations, but `on_method_invocation_observed` is
  called only from the interpreter's uncached-invocation paths. Once a method
  is compiled at C1 and cached, dispatch stops going through them, so the
  counter stops advancing and the C2 recommendation is never reached. This is
  the `wire-tiered-manager` question, and it is the one that decides whether
  any of the above matters.
* **Allocation is a GC-capable point with no safepoint map.** `Op::New` does
  not call `emit_safepoint_map`, so an IR frame that allocates leaves the
  sp-id slot naming an earlier safepoint. It is fail-closed today (the reload
  has already retracted that safepoint's shadow publication, so the band scan
  refuses the cycle), but it is a coverage hole, and it is a prerequisite for
  lowering array allocation.

## Reproducing any of this

```bash
CRATONVM_JIT_FORCE_C2=1 CRATONVM_DBG_IR_COMPILES=1 cratonvm -cp bench BinTreesClassic 16
```

Every pipeline stage names its own refusal. An absence is not a reason — that
is the single most expensive lesson in this document's history, and it cost
three separate wrong conclusions before the reporting existed.

---

# Original document (retired)

# The optimizing IR (C2) tier is disabled by default — `moving_young` turned it off

**Status:** 🟢 **FIXED 2026-07-30** by scoping the gate (option 2 below) —
but see the **2026-07-31 correction** at the bottom: the same scoping was
applied to a SECOND gate (raw JIT-to-JIT direct calls) where it is unsound, and
that half is reverted. The optimizing tier does run on default flags.

## Fix

The gate asked the wrong question. It read `!x64::moving_young_enabled()`, but
what it protects against is a mapless IR frame being live while the collector
**relocates** — and relocation cannot observe a compiled frame at all today:
`conservative_roots::refresh_moving_young_coverage_for_current_thread` vetoes
moving-young process-wide as soon as `jit_code_range_count() != 0`, and
`memory::roots::collect_roots` runs that refresh on the path of every
collection. So a relocating cycle happens only while the process holds no
compiled code. The gate was disabling the optimizing tier in exchange for a
hazard that could not occur.

`types::flags::JIT_PUBLISHES_RELOCATION_CONTRACT` (`false`) now names that fact,
and `x64::moving_young_relocates_compiled_frames()` = `moving_young_enabled() &&
JIT_PUBLISHES_RELOCATION_CONTRACT` is what the two relocation-safety admission
gates read — this one and `direct_jit_callee_calls_enabled`. The runtime veto
reads the same constant, so a future change cannot lift the veto while leaving
a gate disarmed. That is option 2 ("scope the gate") from the list below, done
in a way that keeps option 1 the eventual answer.

Map-publication machinery is deliberately untouched: `shadow_stack_maps_enabled`
and `collect_live_oop_homes` still key on `moving_young`, so the single-pass
backend keeps emitting and exercising the protocol a future flip depends on.
Scoping those too was implemented, measured as no-change, and reverted — see
the note in `shadow_stack_maps_enabled`.

### Verification (default flags; these previously required `CRATONVM_NO_MOVING_YOUNG=1`)

| target | before | after |
|---|---|---|
| `cratonvm-jit --lib` | 1043 passed / 13 failed | **1058 / 0** |
| `jit tests/ir_vs_singlepass.rs` | 77 passed / 12 failed | **89 / 0** |
| `cratonvm-jit` all targets | — | **every target 0 failed** |
| `cratonvm-gc --lib` | — | **873 / 0** |
| `cratonvm-vm --lib` | — | 2300 / 1 (pre-existing `/tmp` Unix-socket test on Windows) |

`BinTreesClassic 18` returns the HotSpot checksum `68332206` at both `-Xmx2g`
and `-Xmx512m`, and `[GC] moving_young: cycles=0` with every fallback still
`jit-relocation-contract-unproven` — i.e. the collector's proof is unchanged and
relocation remains vetoed. The fix removes a JIT self-handicap; it does not
weaken the GC.

Real-workload effect, Hibernate on default flags:

| class | before | after | HotSpot |
|---|---:|---:|---:|
| `ASTParserLoadingTest` | 376 s | **~190 s** | 21 s |
| `OracleInlineMutationStrategyIdTest` | 322 s | **188 s** | 41 s |
| `jpa.lock.LockTest` | 34.5 s | **18.5 s** | 10 s |

The first two now fit inside the suite's 300 s per-class cap. The
`type.temporal` pair still exceeds it on default flags and still passes under
`CRATONVM_NO_MOVING_YOUNG=1`, so a residual moving-young cost remains outside
these two gates — `flush_scratch_registers` at every safepoint
(`emit_pre_safepoint_spill_impl`) and the `can_elide_self_call_register_spill`
proof are the untested candidates. Not yet measured: the box had nine other
sessions' VMs running.

---

## Original report

**Status at filing:** 🔴 OPEN. Not a regression in the IR pipeline itself; a
consequence of an unrelated default flip that nothing flagged, because the test
suite that would have caught it could not compile at the time.

## The finding

`try_compile_inner` (jit/src/lib.rs) admits the optimizing IR pipeline only
when:

```rust
if optimize
    && !x64::moving_young_enabled()
    && ir::ir_compatible(&scan)
    …
```

and `types/src/flags.rs` now says:

```rust
pub const DEFAULT_MOVING_YOUNG: bool = true;
```

So on a default run `moving_young_enabled()` is `true`, the gate is `false`, and
**every compile falls through to the single-pass (C1) backend. The C2/IR tier
never runs.**

The gate is legitimate and should not simply be deleted — its comment states
the reason: *"IR lowering has no exact-RBP or safepoint-map publication. A
mapless IR frame can be live when the moving young collector runs, but cannot
prove or rewrite its roots."* Under a relocating young generation that is a
correctness requirement, not a tuning knob.

What is not legitimate is that the two landed independently and nothing
reported the interaction.

## Evidence

`tests::step3_optimize_toggle_routes_c1_singlepass_and_c2_ir` calls the very
same `try_compile` entry production uses, with `optimize = true`, and counts IR
lowerings through the `IR_LOWER_COMPILES` thread-local:

```
assertion `left == right` failed: optimize=true (C2) must route `add` through the IR pipeline
  left: 0
 right: 1
```

`left: 0` — with default flags, an `optimize = true` compile produces **no** IR
body. Setting `CRATONVM_NO_MOVING_YOUNG=1` and changing nothing else flips it to
`1` and the test passes. The same single flag turns **24 of the 25** failing
`cargo test -p cratonvm-jit` assertions green:

| target | default flags | `CRATONVM_NO_MOVING_YOUNG=1` |
|---|---|---|
| lib | 1043 passed / 13 failed | 1054 passed / 2 failed |
| `tests/ir_vs_singlepass.rs` | 77 passed / 12 failed | 89 passed / 0 failed |

## Why nobody noticed

Three things had to line up:

1. `DEFAULT_MOVING_YOUNG` flipped to `true` (`67de5400a`), disabling the gate.
2. `service_callee_deopt` / `set_throw_bci` were added to `JitRuntimeHelpers`
   without updating the ten `jit/tests` tables, so **`cargo test -p
   cratonvm-jit` stopped compiling at all** — 11 × `E0063`. Fixed in
   `4d8a39a39`.
3. Two SIGSEGVs then killed the lib binary at test ~828 and
   `ir_vs_singlepass.rs` partway through. Fixed in `fcc723007`.

So from the moment the flip landed, the tests that assert "C2 routes through
IR" could not run to report it.

## Consequences to check

This is the plausible common cause behind several open throughput documents,
and they should be re-read with it in mind rather than treated as independent:

* `docs/known-issues/tomcat/30-…-OPEN.md` — "1531/1642 hot methods never
  compile" and the whole hot-loop tiering story.
* `project_hib_actionqueue_graph_default_jit_tiering_blocker` — "1362/1463 hot
  methods never compile".
* The `wire-tiered-manager` work, whose try/catch C2 admission fix was recorded
  as *"routing proven, no speedup yet"* — consistent with routing that reaches a
  tier which is then globally switched off.

Note this does **not** mean methods stop compiling: the single-pass C1 backend
still compiles them. It means the optimizing tier contributes nothing, so every
C2-only optimization (the IR optimizer, its inline caches, its direct-call
lowering) is inert by default.

### Measured cost on a real test (2026-07-30)

[tomcat/32.1](../internal/fixed-suite-bugs/tomcat/32-doc04-residual-perf-assertions-CLOSED.md) is the first item to
quantify this end to end. Its workload is a nested chain of ordinary instance
methods — `MappingData.recycle()` → 4× `MessageBytes.recycle()` → 2×
`AbstractChunk.recycle()` per iteration — exactly the shape both penalties bite
on. On a dev build carrying the `jit_virtual_tierup` fix, two interleaved passes:

| | default | `CRATONVM_NO_MOVING_YOUNG=1` |
|---|---|---|
| `MapperPerfProbe -mode recycle` | 2.48 / 2.51 µs | **0.80 / 0.79 µs** |
| `CalleeTierUpProbe` | 2 422 / 2 375 ns | **617 / 677 ns** |
| `TestMapperPerformance` loop, easy host | 5 724 ms | **2 262 ms** |
| `TestMapperPerformance` loop, hard host | 8 164 ms | **4 052 ms** |

**~3.1–3.7×** — and it is the difference between that test failing its absolute
5 000 ms budget on both hostnames and passing on both. The second penalty
matters as much as the tier here: `direct_jit_callee_calls_enabled()` also
returns `false` under moving-young, so every compiled call in that chain takes
the dispatch bridge instead of a direct JIT-to-JIT edge.

**Diagnosing it from a trace:** `CRATONVM_DBG_JITC=1` shows
`MappingData.recycle` emitting `len=3569` at *both* `tier=C1 optimized=false`
and `tier=C2 optimized=true`. Byte-identical output across the two tiers is the
tell that the C2 compile is a relabelled C1.

**Trap:** the disabling lever is `CRATONVM_NO_MOVING_YOUNG=1`, named in
`moving_young_disables_optimizing_tier`'s own warning text.
`CRATONVM_MOVING_YOUNG=0` is *not* it — it silently changes nothing and reads
exactly like a successful elimination, which is how tomcat/32 first ruled this
cause out in error.

## What a fix would involve

1. **Give IR the map contract the gate demands** — exact-RBP + a complete
   rewritable root map at every GC-capable safepoint, i.e. the same protocol the
   single-pass backend already publishes. Then the gate can be dropped honestly.
2. **Or** scope the gate: if `precise_jit_maps_enabled() || moving_young_enabled()`
   already forces the precise-map machinery on (jit/src/x64.rs does exactly this
   for the single-pass path), establish whether IR bodies can join it rather
   than being excluded wholesale.
3. Until then, **make the interaction loud**: a compile-time or startup
   diagnostic saying "optimizing tier disabled: moving_young" would have turned
   this into a one-line observation instead of a cross-suite archaeology
   exercise.

Do **not** "fix" it by flipping `DEFAULT_MOVING_YOUNG` back — that trades a
throughput ceiling for a GC-correctness hazard, which is the wrong direction and
reverses a deliberate architecture decision
(`docs/internal/arch-2026-07-26/moving-young-precise-roots.md`).

## Update 2026-07-30 — item 3 landed; the trade is now real, not theoretical

`moving_young_disables_optimizing_tier()` (jit/src/lib.rs) replaces the bare
`!x64::moving_young_enabled()` term at the admission site. It returns the same
answer and, the first time it vetoes, emits one `warn` naming the cause, the
consequence, and the opt-out. Observed on a Tomcat `TestTomcat` run:

```
WARN cratonvm_jit: [jit] optimizing (C2/IR) tier DISABLED: the moving young
generation is active and IR lowering publishes no exact-RBP or per-safepoint
oop map, ... `CRATONVM_NO_MOVING_YOUNG=1` restores the optimizing tier and
gives up compaction.
```

**What changed underneath it.** Until 2026-07-30 this document described a trade
that was not actually being made: the moving young generation was
default-*requested* but could never *engage* under JIT, so a process paid the
optimizing tier for compaction it never received. Three defects caused that
(see `docs/internal/default-moving-young-enabled-20260730.md`) and all are fixed —
`BinTreesClassic 18` at `-Xmx512m` now runs 25 real Cheney young cycles with
zero coverage fallbacks. The cost recorded here is now buying something, so the
comparison a reader should make is three-way, not two-way:

| `-Xmx512m` bt18 | young collector | optimizing tier | result |
|---|---|---|---|
| default | moving, 25 cycles | off | 4,220 ms |
| `CRATONVM_MOVING_YOUNG_NO_JIT=1` (the pre-fix behaviour) | non-moving, on the moving-young heap policy | off | 4,295 ms |
| `CRATONVM_NO_MOVING_YOUNG=1` | non-moving | **on** | 4,279 ms |

Medians of five interleaved rounds. The three are indistinguishable, so on this
workload the optimizing tier is currently worth nothing measurable either —
which is itself a reason to price item 1 before assuming it is.

**Item 2 is more tractable than it looks, and for a specific reason — but it is
still not the answer.** An IR-lowered `CompiledMethod` publishes no `oop_maps`
and no `sp_id_slot_off`, so `conservative_roots::moving_young_frame_coverage_complete`
returns `false` for any live IR frame *by construction*: the per-cycle proof
already fails closed on exactly the condition the gate exists to prevent.
Admitting IR would therefore not be unsound — it would mean every cycle with a
live IR frame diverts to the non-moving sweep. Since IR compiles the hottest
methods, that is close to "moving-young never engages again", which is why it
is **not** done here. C2 and a relocating young generation are mutually
exclusive in practice until IR supplies the map contract (item 1); choosing
between them is a policy decision with measurable stakes on both sides, and the
warning above now makes the choice visible instead of silent.

## Test-side follow-up already landed

The 24 affected tests exercise IR *lowering*, not the deployment policy, so they
now pin it explicitly via `x64::set_moving_young_override(Some(false))` (a
thread-local, never set in production) instead of silently depending on the
ambient default. That restores their coverage and, more importantly, makes the
dependency visible at each call site — the absence of which is what let this go
unseen.

## 2026-07-31 correction — the direct-call half of the scoping was unsound

The scoping above applied one predicate to **two** gates. Splitting them was
required: the optimizing-tier half is correct and stays, the raw JIT-to-JIT
direct-call half is reverted to the bare `moving_young_enabled()` flag.

### What broke

With both gates open on default flags,
`org.springframework.boot.webmvc.autoconfigure.error.BasicErrorControllerIntegrationTests`
**SIGSEGVs on every run** — 8/8 on a pristine `90652574cf` build, 14/14 on a
later tip:

```
SIGSEGV at pc=0x…, addr=0x20040000000
  r11=0x20040000000   slot[r10]: 0x0 0x0 0x0 0x0 0x0 0x0 0x0 0x0
  fault pc is in NO live registered code buffer
```

A read through a heap slot that reads back all zeros — the *reclaimed-root*
signature, not a relocation one. It came with a flood of ~480
`JIT try_patch_i32: offset out of bounds; marking buffer overflowed` warnings
per run (the IR lowerer's buffer estimate does not budget for the direct-call /
inline-IC sequences the open gate makes it emit; each such compile is discarded
and falls back to C1).

### Isolation

| configuration | result |
|---|---|
| default (both gates open) | **SIGSEGV**, 8/8 and 14/14 |
| `CRATONVM_JIT_DIRECT_CALLEE_CALLS=0` (optimizing tier still on) | **clean**, 26/26 tests, 5/5 runs |
| `CRATONVM_JIT_IR_DIRECT_CALL=0` (IR half only) | **SIGSEGV**, 3/3 |
| `CRATONVM_NO_MOVING_YOUNG=1` | inconclusive — the class does not finish inside 900 s in this configuration |

So the optimizing tier itself is fine; the defect is in the **single-pass**
backend's raw JIT-to-JIT edge, which this gate had kept dark for months and
which the scoping switched on for the first time in production.

### Why the original argument does not hold

The scoping argument was "the hazard needs a RELOCATING collection, and the
runtime veto forbids one while such a frame is live". But the hazard this gate
guards is stated at the emission site in `x64.rs` and is broader than
relocation:

> The inline MIC/PIC path emits a raw CALL into another compiled body. That
> bypasses the interpreter-owned JitEntryGuard, leaving the active-RBP mirror
> pointing at the callee while the root-chain metadata still names the caller.
> A GC at that boundary can therefore select an incompatible oop map and
> **reclaim a live root**.

Reclaiming a live root does not require the collector to move anything, so
`moving_young_relocates_compiled_frames()` is the wrong predicate for this gate.
The optimizing-tier gate is different: those frames DO carry a guard, so they
are reachable from the entry chain and the relocation-scoped predicate is the
right question there. **Two gates, two questions — they must not share a
predicate**, which is now pinned by
`x64::tests::direct_jit_callee_calls_stay_gated_off_under_moving_young`.

### Status of the edge itself

Re-gated, **not fixed**. Reopening it needs an unguarded callee frame to be
describable to the root scan first (`chain_entry_rbp_is_foreign` in
`vm/src/jit/conservative_roots.rs` detects the situation and gives up coverage;
that is a fallback, not a fix). Acceptance gate for any future attempt: this
class, 14 consecutive runs, on default flags.

### What the re-gating costs — adopted 2026-07-31 from the retired tomcat/32

`catalina.mapper.TestMapperPerformance` is this gate's clearest price tag, and
it now lives here rather than in
[tomcat/32](../internal/fixed-suite-bugs/tomcat/32-doc04-residual-perf-assertions-CLOSED.md),
because once that document's cause 1 was fixed **this gate became the entire
remainder**. The test asserts each of **nine** hostnames completes 10^6
`recycle() + map()` calls in under 5 000 ms. `MapperPerfProbe`, hostname
`xxxxxxxxxxx`, 300k iterations:

| loop body | dev `b695d468f` (pre-regression, PASSED) | 2026-07-31 dev | 07-30 with both penalties removed |
|---|---|---|---|
| `MappingData.recycle()` | 0.73 µs | **3.46 µs** | 0.80 µs |
| `recycle()` + `map()` | 2.03 µs | **6.2–8.5 µs** | — |

The class fails on the first and easiest hostname at 6 057 ms, reruns at
9 222 ms, and never reaches the other eight. Its workload is
`MappingData.recycle` → 4× `MessageBytes.recycle` → 2× `AbstractChunk.recycle`
per iteration — exactly the shape this gate penalises — and the trace says so
directly:

```
[cratonvm-jitc] ir-direct-call MISSED java/util/Calendar.setTime(...)V
                @pc=5 ir_direct=false static=false special=false
```

tomcat/32 predicted "32.1 should close when that branch merges". The branch
merged, half of it was reverted here four days later for the soundness reason
above, and nothing re-tested the prediction — so it stayed recorded as
closing-soon while still failing. It closes when this edge does.

**Correction to this document's own 07-30 entry.** The "Measured cost on a real
test" table above was produced with `CRATONVM_NO_MOVING_YOUNG=1`. That lever
now **crashes**: access violation, 3 runs of 3 on a ten-second probe
(`DateSymbolsProbe`), with `CRATONVM_JIT_DIRECT_CALLEE_CALLS` on *or* off,
while the default lane is clean 3/3 on the same binary. So that table cannot be
reproduced today and should be read as history, not as evidence. It is not this
edge — disabling the edge does not change the crash — and is filed separately.

Whoever reopens the edge should re-run `TestMapperPerformance` — the whole
class, not two of its nine hostnames — as a second acceptance signal alongside
the Spring class above. The edge now has its own document,
[jit-raw-jit-to-jit-shadow-stack-overflow-20260731](jit-raw-jit-to-jit-shadow-stack-overflow-20260731.md),
which supersedes the reclaimed-root framing above; this table is what reopening
it is worth on a real Tomcat class.

## Correction 2026-07-31 — "the optimizing tier does run on default flags" is not what it sounds like

The scoping above is right and it did open the gate. What it did not do — and
what the header of this document implies — is make the optimizing tier
*produce bodies*. Measured with `CRATONVM_DBG_IR_COMPILES=1`, a default run
produces approximately none, for two reasons that have nothing to do with
moving-young:

1. **`optimize=false` on nearly every compile.** The tier is selected outside
   `cratonvm-jit`; the eager first-call single-pass compile caches a non-IR
   body and preempts every later path. The gate this document scoped sits
   *downstream* of a decision that rarely asks for C2 at all.
2. **`compact_ref_fields_enabled()` (default TRUE) made `IrBuilder::build`
   refuse every `getfield`/`putfield`** — most object-oriented Java — at stage
   one, silently. Fixed 2026-07-31; the constraint now lives where the
   layout-naive displacement is emitted.

Both are documented in `docs/internal/jit-ir-relocation-map-contract.md`, along
with the per-stage refusal reporting that makes this checkable instead of
inferable. The practical consequence for THIS document: its verification table
shows unit-test counts, which the scoping genuinely fixed, but nothing in it
establishes runtime C2 coverage — and the ASTParser 376→138 s and Oracle
322→93 s improvements credited to the scoping cannot have come from the
optimizing tier, since it emitted nothing. `direct_jit_callee_calls_enabled`,
scoped in the same commit and since re-gated, is the candidate and still needs
re-measuring.
