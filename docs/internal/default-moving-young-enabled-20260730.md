# Moving young generation default-on delivery (2026-07-30)

## Outcome

The generational collector's moving/compacting young path is the supported
default **and it now runs**. `CRATONVM_NO_MOVING_YOUNG=1` is the compatibility
opt-out, and an incomplete per-cycle JIT root-coverage proof still diverts that
individual collection to the non-moving sweep.

`DEFAULT_MOVING_YOUNG` had been `true` on `origin/dev` since
`67de5400ac60d7d4097b0657bbfac9965cbd744a` (it arrived while fixing Liquibase GC
corruption). The first half of this delivery made the repository state
deliberate — pinning the empty-environment result, covering the opt-out, and
correcting the stale default-off documentation.

**The second half found that the flip was nominal.** With the constant `true`,
every young collection in any process that had compiled a single method still
ran the non-moving sweep. Measured on `BinTreesClassic 18` at `-Xmx512m`:
`cycles=0 coverage_fallbacks=66`. Three independent defects produced that, and
all three are fixed here. The same lane now reports `cycles=25
coverage_fallbacks=0` with the HotSpot checksum. Throughput on these workloads
is unchanged — see "Throughput: neutral" below, including a claim made earlier
in this document that the final `origin/dev` merge disproved.

## Defect 1 — a process-wide blanket bypassed the per-cycle proof

`refresh_moving_young_coverage_for_current_thread` opened with

```rust
if cratonvm_jit::jit_code_range_count() != 0 {
    mark_moving_young_coverage_incomplete_because(JIT_RELOCATION_UNSUPPORTED);
    return false;
}
```

added by `86e69e848` (2026-07-29) after a Hibernate corruption chase. The
existence of *any* compiled code — not a specific unproven frame — refused every
moving collection, so the entire per-frame verifier below it was unreachable in
production.

Its stated rationale was that "discovering a gap while scanning is too late to
repair that collection". That does not describe the wiring: this function *is*
the pre-cycle proof. `memory/roots.rs::collect_roots` calls it (through
`refresh_moving_young_coverage_for_collection`) **before** the collector selects
a young path, and `gen_heap::collect_garbage_inner` diverts on the published
verdict. A gap found here has always been found in time.

It is now the `CRATONVM_MOVING_YOUNG_NO_JIT=1` knob — the same fail-closed
direction, no longer the only available setting. Keeping it is worthwhile: it
makes "is this a moving-young defect?" a single-variable experiment, and it is
the correct emergency lever if a workload exposes an obligation the verifier
does not model.

## Defect 2 — the coverage proof erased its own input

With the blanket off, **100%** of fallbacks became `missing-exact-rbp`, and the
diagnostic showed `top_rbp=0x0` for a frame that had 7 oop maps and a live
sp-id slot. Nothing was wrong with the frame.

`prune_returned_jit_entries` ended with an unconditional
`reload_top_rbp_cache(&v)`. The innermost-RBP mirror is the **live** value —
each compiled prologue writes it with an inline `mov gs:[disp], rbp` (or through
the `jit_frame_record` helper). `PreciseFrameInfo::exact_rbp` is only a
*snapshot* of that mirror, taken in `push_entry_full` at the moment an entry
stops being top. For the entry that is *currently* top, nothing has ever written
that field, so it still holds the `0` from `enter_with_compiled`.

`refresh_moving_young_coverage_for_current_thread` prunes first and reads the
mirror second. A prune that removed nothing therefore overwrote the live RBP
with that `0` on the way in, and the proof then failed itself.

The reload now runs only when pruning actually changed the top.

### Its history is worth reading, because the symptom changed shape

- **2026-06-17, `c9b56f7d6`** moved the hot per-invocation RBP write into a TLS
  mirror for throughput and left the cold reload reading `exact_rbp` as if that
  field were still authoritative.
- **2026-07-01, `a5623891c`** made the moving-young coverage refresh call
  `prune_returned_jit_entries`, which put the clobber directly on the GC path.
- **2026-07-01 → 07-26** there was no `MISSING_EXACT_RBP` obligation yet, so a
  zero `exact_rbp` was not detected. Moving cycles *ran*, and
  `remap_active_jit_frames` — which requires `info.exact_rbp != 0` — silently
  **skipped the innermost compiled frame**. A copying collection that does not
  rewrite the innermost frame's oops is precisely the "moving young gen drops
  JIT-held oops" failure class, and this is consistent with the corruption
  reports that drove the 07-26 and 07-29 work, though it is not proof that it
  was their only cause.
- **2026-07-26, `9494c0680`** introduced `MISSING_EXACT_RBP`. From that point the
  proof correctly refused to relocate — so the defect stopped corrupting and
  started merely disabling, which is how it survived a "DEFAULT-ON" status line.
- **2026-07-29, `86e69e848`** added defect 1's blanket on top, hiding the reason
  behind `jit-relocation-contract-unproven`.

The lesson worth keeping: a *detection* added for a corruption can turn an
unsound feature into an inert one without anyone noticing the difference,
because both states pass every correctness test. Only the cycle counter tells
them apart, which is why `moving_young: cycles=N` is now printed next to the
fallback count rather than derived on request.

Regression test: `a_no_op_prune_does_not_clobber_the_live_rbp_mirror`.

## Defect 3 — recursion was misread as an unguarded foreign frame

With defect 2 fixed, 100% of fallbacks moved to
`innermost-rbp-belongs-to-unguarded-callee`. `chain_entry_rbp_is_foreign`
rejected any frame whose return address pointed into registered JIT code, on the
grounds that a chain entry names a boundary method and cannot describe a callee
reached by a guardless JIT→JIT call.

But a compiled method that **recurses** does so through a direct `E8 rel32` CALL
back to its own entry (`x64.rs`, `self_call_patches`), pushing no guard — so any
recursive Java workload leaves the mirror pointing at an inner activation of the
very method the entry names, which `info.compiled_method` describes exactly.
`BinTreesClassic`'s `itemCheck`/`bottomUpTree` recursion made this 82/82 of the
fallbacks at depth 18.

The check now recognises that case from the machine code rather than inferring
it from which direct-call features happen to be gated off: the return address
must lie in the entry's own body **and** the five bytes ending there must be
`E8 rel32` resolving to that body's entry point. An indirect call, a call from a
different method, or bytes that cannot be read all stay foreign. Regression
test: `direct_self_call_return_is_recognised_only_for_a_real_e8_to_the_entry`.

## Runtime evidence

Release binaries `cvm-myd-r4.exe` (pre-merge) and `cvm-myd-r5.exe` (after the
final `origin/dev` merge), both built under unique names so no measurement can
borrow a stale image. The table is the r4 lane sweep; the wall times in it are
superseded by the interleaved figures below, the cycle/fallback/checksum columns
are not (r5 reproduces them exactly).

| Lane | Heap / depth | moving cycles | fallbacks | checksum | time |
|---|---|---:|---:|---|---:|
| default | 512m, 18 | **25** | **0** | 68332206 ✓ | 4,260 ms |
| `CRATONVM_MOVING_YOUNG_NO_JIT=1` | 512m, 18 | 0 | 64 | 68332206 ✓ | 15,789 ms |
| `CRATONVM_NO_MOVING_YOUNG=1` | 512m, 18 | — | — | 68332206 ✓ | 4,986 ms |
| default | 128m, 16 | **22** | **0** | 14985902 ✓ | 851 ms |
| default | 256m, 14 | **2** | **0** | 3222190 ✓ | 176 ms |
| default | 8g, 18 | **1** | **0** | 68332206 ✓ | 3,315 ms |

Three consecutive repeats of the 512m lane returned `cycles=25
coverage_fallbacks=0` and the HotSpot checksum every time — the engagement is
deterministic, not a race that happened to land. The table above was taken
before the final `origin/dev` merge; after it the cycle counts, fallbacks and
checksums are identical (25 / 22 / 2 / 1, all zero, all matching).

### Throughput: neutral, and an earlier claim here was wrong

On the **pre-merge** tree the `MOVING_YOUNG_NO_JIT` lane took 15,789 ms against
the default's 4,260 ms, and five interleaved rounds at 512m killed *every*
`MOVING_YOUNG_NO_JIT` round with `OutOfMemoryError`. That looked like a large
win and was written up as one.

It does not survive the merge of the 57 `origin/dev` commits that landed during
this work. Re-measured on the merged tree, five interleaved rounds at 512m:

| lane | rounds (ms) | median |
|---|---|---:|
| default (moving, 25 cycles) | 4760, 4226, 3976, 4120, 4220 | 4,220 |
| `CRATONVM_MOVING_YOUNG_NO_JIT=1` | 4836, 4295, 3978, 3935, 4820 | 4,295 |
| `CRATONVM_NO_MOVING_YOUNG=1` | 4712, 4171, 4912, 4224, 4279 | 4,279 |

No lane OOMs and the three are indistinguishable. The Hibernate gauntlet says
the same once the baseline is repeated rather than trusted: default 492 s, then
blanket 377 s, then **default again 371 s** — the apparent 1.3–1.5× in either
direction was shared-host load, not the collector.

So on the merged tree this work is **throughput-neutral on these workloads**.
What it delivers is the property the feature exists for: the young generation
actually compacts, which is what makes small heaps viable
(`docs/moving-young-throughput.md` records the case where the non-moving path
dies with `OutOfMemoryError: young gen exhausted` at `-Xmx512m` and the
compacting one completes). Anyone quoting a speedup from this change should
re-measure interleaved, on a quiet host, and repeat the baseline — both of the
misleading figures above came from not doing that.

## Cross-thread coverage is still an open obligation (by design)

`probes/MovingYoungConcurrentProbe.java` puts four threads in long-lived
compiled frames, allocating hard enough to collect from each of them. Its
checksum is order-independent, so HotSpot and CratonVM must agree exactly. Run
as `MovingYoungConcurrentProbe 4 400 2000` at `-Xmx256m`.

| Lane | checksum | moving cycles | fallbacks |
|---|---|---:|---|
| HotSpot JDK 25 | 3852744000 | — | — |
| default | 3852744000 ✓ | 0 | `cross-thread-jit-peer` = 3 |
| `CRATONVM_MOVING_YOUNG_NO_JIT=1` | 3852744000 ✓ | 0 | `jit-relocation-contract-unproven` = 3 |
| `CRATONVM_NO_MOVING_YOUNG=1` | 3852744000 ✓ | — | — |

`refresh_moving_young_coverage_for_collection` treats a cycle as unproven
whenever a peer thread is in compiled code, because a peer's registers and frame
slots are not rewritable by this collection. That is obligation #8 in
`docs/internal/arch-2026-07-26/moving-young-precise-roots.md` ("cross-thread
coverage handshake") and it is not implemented. So single-threaded phases now
compact and multi-threaded phases still take the non-moving sweep — visibly
accounted rather than silently. Closing it is the next item, and the largest
remaining throughput lever for Tomcat/Spring-shaped workloads.

## Test and suite evidence

- `cargo test -p cratonvm-gc --lib` — 873 passed, 0 failed.
- `cargo test -p cratonvm-types` — 422 passed, 0 failed across all targets. The
  `flag_surface` fixture had been failing for two reasons unrelated to
  moving-young, both fixed here: a checked-in UTF-8 BOM made the first variable
  unreachable, and `CRATONVM_SYNTHETIC_QUARKUS_START` had been added to the
  inventory without the fixture.
- `cargo test -p cratonvm-jit --lib` — 1,053 passed, 0 failed.
- `cargo test -p cratonvm-jit --test ir_vs_singlepass` — 89 passed, 0 failed.
- `cargo test -p cratonvm-vm --lib` — 2,449 passed, 111 ignored, 6 failed. The
  same 6 fail with these changes stashed (skip-list classification ×3, the
  interpreter panic census, the interpreter B3 gate, and the attach-listener
  socket baseline), so they are pre-existing and unrelated.
- HotSpot-differential lane — **20/20 matched in each of three modes**: default,
  `CRATONVM_NO_MOVING_YOUNG=1`, and `CRATONVM_MOVING_YOUNG_NO_JIT=1`. The same
  lane on the pre-change binary also matched 20/20, so the baseline is a real
  control and not an empty one. The lane runs each of
  `cratonvm.{DiffArithmetic, DiffFloatFormat, DiffLocaleCase, DiffNpeMessage,
  DiffString, IntrinsicDiff, IntrinsicMegamorphic, IntrinsicVirtualGuard,
  JitCollectionCtorIdentity, JitDeepRecursionFaultRecovery, JitDifferential,
  JitExceptionTableInlineCache, JitNull, JitOsrLoopProgress, NestedClinitStartup,
  RealAnnotations, RealAqs, RealFjp, RealRaf, SyntheticDiff}` from
  `vm/tests/resources` under both VMs and compares stdout byte-for-byte, after
  dropping CratonVM-only diagnostics (`tracing` records and the launcher's
  `[cratonvm]`/`[GC]` lines) and normalising CRLF — MSYS `grep`/`sed` strip CR
  from one side of the pipeline but not the other, which otherwise reports every
  line as divergent.
- Tomcat `org.apache.catalina.startup.TestTomcat` — `OK (26 tests)`, matching
  the recorded baseline.
- Hibernate ORM, first 120 classes of `passed.txt`, 4 shards — **119 PASS, 1
  FAIL**, on both the pre-merge and merged trees and in both lanes. The single
  failure,
  `org.hibernate.orm.test.action.queue.integration.DeferredIdentityGenerationIntegrationTest`,
  fails identically in the `CRATONVM_MOVING_YOUNG_NO_JIT=1` control, so it is
  pre-existing (it belongs to the open `action.queue` tiering item). Wall times
  are in the throughput section above; they carry no signal.
- Spring Boot was **not** run: the checkout on this box cannot configure
  (`build-plugin/spring-boot-antlib` is missing, so `-RefreshClasspaths` fails
  during Gradle configuration). That is a fixture gap, not VM evidence in either
  direction.

## Safety contract

Default-on does not authorize relocation by itself. A live compiled frame must
still publish complete rewritable oop homes for the active safepoint. A missing
exact frame base, a genuinely foreign innermost frame, an unregistered entry, an
unpublished band word, a wide-locals gap, or a peer thread in compiled code each
record a reason and run the non-moving sweep for that cycle. The explicit
opt-out overrides the compatibility opt-in across the whole
codegen/root/collector contract. Interpreter-only collections remain relocatable
because all of their roots are rewritable.

The per-reason histogram (`[GC] moving_young_fallback_reason:` under
`CRATONVM_DBG=gc-stats`) is what turned this delivery from archaeology into
three successive one-line answers, and it is the first thing to read if
moving-young ever appears to stop engaging again.
