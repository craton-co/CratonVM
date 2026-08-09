# `CRATONVM_NO_MOVING_YOUNG=1` crashes — root publication was keyed on the collector

**Status: ✅ CLOSED 2026-08-03.** Both faults are fixed and the lane is usable
again. Fault 1 (root publication withdrawn with the collector) was fixed
2026-07-31 in this document's own branch. Fault 2 — filed here as OPEN, with a
"where to start" that pointed at the wrong mechanism — was already fixed five
hours later the same day by `7f1b1f263`, from the other end of the same
investigation, and nobody connected the two until now. Supersedes
[`no-moving-young-lever-crashes-SUPERSEDED-20260731.md`](no-moving-young-lever-crashes-SUPERSEDED-20260731.md),
which found the same lane broken from a different workload.

Retired from `docs/known-issues/jit/`.

## Why the lane matters

`CRATONVM_NO_MOVING_YOUNG=1` (grouped spelling: `CRATONVM_GC=-moving-young`) is
the standard A/B lever for anything moving-young-related — it is named in
`moving_young_disables_optimizing_tier`'s own warning text and it produced the
cost tables in the retired
[moving-young gate doc](jit-optimizing-tier-moving-young-gate-RETIRED-20260731.md)
and in tomcat 32.1. While it crashed, none of those tables could be re-derived,
and any new measurement that reached for it read as an unrelated failure.

## Fault 1 — publication withdrawn with the collector (FIXED 2026-07-31)

`shadow_stack_maps_enabled()` (`jit/src/x64/licm.rs`) and its mandatory twin
`conservative_roots::shadow_stack_enabled()` (`vm/src/jit/`) both read

```rust
flags().jit.shadow_stack || (moving_young_enabled() && …)
```

The shadow stack is how a JIT frame's live references become visible to the
collector at all. Keying it on `moving_young_enabled()` made **root visibility
a property of which young collector runs**, so the opt-out silently withdrew
it. On a pristine `origin/dev` build (`9fcd1b63f`), Hibernate
`ZonedDateTimeTest` and `OffsetDateTimeTest` then SIGSEGV'd in **1–3 s**,
`addr=0x0`, fault pc inside a live JIT code buffer, `slot[r10]: 0x0` — the
reclaimed-root signature. The same classes **pass on default flags**.

Single-variable proof: `CRATONVM_NO_MOVING_YOUNG=1 CRATONVM_SHADOW_STACK=1`
— publication restored, nothing else changed — ran clean past the 200 s
cut-off, 2/2.

**Fix:** the `moving_young_enabled() &&` term is removed on **both** sides in
one change, leaving `CRATONVM_JIT_MY_SHADOW_EMISSION=0` as the joint opt-out.
The default path is byte-identical (moving-young is on by default, so the
expression already evaluated true). The same day
`reg_spill_for_root_visibility` in `x64.rs` was un-keyed from `precise_maps`
for the identical reason: three root-visibility mechanisms were all riding on
the collector flag, and the opt-out withdrew all three at once. Pinned by
`x64::tests::shadow_stack_maps_enabled_does_not_depend_on_the_young_collector`,
which additionally asserts publication survives a moving-young override.

## Fault 2 — the PIC cascade's inter-slot `JNE` truncated to `rel8` (FIXED 2026-07-31, confirmed 2026-08-03)

With publication restored the lane still died — `SIGILL`, deterministically.
This document filed it as a "one-byte-off control transfer into the safepoint
spill run" of unknown origin and proposed looking at recorded
`native_pc_offset`s, OSR entries and exception-handler resumes. **All three
were the wrong place.** Nothing computed an address; a branch was *encoded*
wrong.

### What it actually was

The inline PIC cascade's inter-slot `JNE` was a `rel8`, sized by a comment
claiming "a single slot body is ~30 bytes". A slot body had since grown the
post-call innermost-RBP republish and the callee-deopt service check, putting
it past 127 bytes. The patch computed the displacement as `i64` and wrote
`rel as u8` — a silent truncation guarded only by `debug_assert!`, i.e.
guarded only where it cannot fire.

Why this lane and not the default: `direct_jit_callee_calls_enabled()` used to
return `false` under moving-young, so `CRATONVM_NO_MOVING_YOUNG=1` is exactly
what **opened** the raw JIT-to-JIT edge — which is what grew the slot bodies
past the `rel8` cliff. The lever did not expose a root-visibility hole here; it
switched on the codegen that overflowed the branch.

That is also why the two "workarounds" in the original isolation table worked,
and why both were misleading: `CRATONVM_NO_PRECISE_REG_SPILL=1` deletes the
blind-spill run, so the wrapped branch has nothing to land in;
`CRATONVM_NO_PRECISE_JIT_MAPS=1` shrinks the slot body back under 127 bytes.
Neither touches the defect.

### The evidence, re-taken 2026-08-03

`cratonvm-nomy-docera`, a release build of this document's own commit
(`58de18d5b`), Hibernate `ZonedDateTimeTest`, `CRATONVM_GC=-moving-young`:
**SIGILL 3/3**, ~30 s in. Under gdb, at the fault:

```
0x…f6e1:  mov  %r8,-0x168(%rbp)      ; the ALL_SPILL_GPRS blind spill
0x…f6e8:  mov  %r9,-0x170(%rbp)      ;   <-- instruction starts here
0x…f6ef:  mov  %r10,-0x178(%rbp)
   …
0x…f761:  mov  (%rsi),%eax           ; receiver class_id
0x…f763:  cmp  (%r10),%eax           ; vs PIC slot 0
0x…f766:  jne  0x…f6eb               ;   <-- 2-byte JNE, rel8
```

`0x…f766` is a **two-byte** `JNE rel8`. Next IP is `0x…f768`, target
`0x…f6eb`, so the encoded displacement is `-0x7D` = `-125`. The true forward
distance to slot 1 was `+131`; `131 as u8` is `0x83`, and `0x83` read as `i8`
is `-125`. That is the truncation, in the encoding, arithmetically exact.

`0x…f6eb` is three bytes into the 7-byte `mov %r9,-0x170(%rbp)` at `0x…f6e8`.
Decoded from there: `90` (a stray `NOP`), then `fe ff` at `0x…f6ec` — not an
instruction. `rip` at the fault is `0x…f6ec`, matching the crash report's
`SIGILL at pc=…6ec` byte for byte, on a different host and a different run from
the one this document was originally written against.

### The fix, and where it came from

`7f1b1f263` (2026-07-31 20:42 UTC), five hours after this document was filed at
18:25 UTC, chasing the *same* truncated branch from the raw JIT-to-JIT
shadow-stack overflow
(`jit-raw-jit-to-jit-shadow-stack-overflow-FIXED-20260731.md`). There the
wrapped branch landed in the pre-call shadow **push** and produced an unguarded
infinite push loop → SIGSEGV; here, with the register spill run also
default-on, it lands in the **blind GPR spill** and produces SIGILL. One
encoding bug, two crash faces, two documents, neither citing the other.

* the PIC inter-slot branch is `rel32`;
* `patch_rel8_or_bail` replaces every remaining `rel as u8` displacement patch —
  a `rel8` that does not fit marks the buffer overflowed so the driver discards
  the method, instead of retargeting the branch;
* `direct_jit_callee_calls_enabled` no longer consults moving-young at all, so
  the two lanes now emit the same slot bodies.

Same classes, `origin/dev` at `63446c7f8` and the branch tip, same host,
interleaved with the doc-era arm in both orders across three batches:
**clean 9/9** (`ZonedDateTimeTest` 6×, `OffsetDateTimeTest` 3×, `ok=404` /
`ok=324`, `failed=0`, 203–349 s each), while the doc-era arm is **SIGILL 7/7**
— never once in the same direction, and never dependent on which arm ran first.

### Residual closed with it — the "also tried, and reverted" note

This document recorded that making the safepoint scratch-register flush
unconditional (`emit_pre_safepoint_spill_impl`, `moving_young_enabled() &&
scratch_flush_at_safepoint_enabled()`) "made things **worse**", turning the
clean lane into a deterministic SIGILL, and blamed the call site for "reserving
spill slots and rewriting `self.stack` at a point the non-precise frame layout
did not budget for".

**That attribution was wrong, and wrong in a way that would have cost the next
person the same week.** The experiment grew the emitted code; growing the
emitted code pushed a PIC slot body past 127 bytes; the truncated branch fired.
Any unrelated change of comparable size would have "reproduced" it.

Re-run on 2026-08-03 with the truncation fixed — the `moving_young_enabled()`
term removed, everything else at `dev`, `CRATONVM_GC=-moving-young`, same
class. Interleaved against the doc-era build as a **positive control**, so a
clean arm cannot be a quiet-window artefact. The control fires at ~30 s, so the
240 s cut-off is ~8× the crash point:

| arm | order | result |
|---|---|---|
| doc-era build (`58de18d5b`) | 1st and 5th | **SIGILL 2/2** |
| `dev` + flush unconditional | 2nd, 4th, 6th | no SIGILL **3/3** |
| `dev` unmodified | 3rd | no SIGILL 1/1 |

Adding the other two interleaved batches, the doc-era arm is SIGILL **7/7** and
no build carrying `7f1b1f263` has SIGILL'd once — including the branch tip
itself, re-validated after the change with the same control in the batch
(`ZonedDateTimeTest` 2×, `OffsetDateTimeTest` 1×, all `failed=0`, control
SIGILL 2/2).

The term still stays, for a reason about the mechanism rather than about a
crash: in the non-moving lane the full-GPR blind spill
(`safepoint_reg_spill_all`, default-on since 2026-07-31) already copies every
caller-saved register into a frame slot the conservative scan reads, so the
flush buys no root visibility there. Under moving-young it is not redundant —
it also rewrites `Compiler::stack` so the *precise* map names those slots, and
a moving cycle has no conservative backstop. Both call-site comments now say
this instead of the old account.

## Hardening added 2026-08-03

`patch_rel8_or_bail` covered the single-pass backend. Three more places still
wrote a displacement through a raw cast:

* `ir_lower.rs` — **eight** unguarded `(a - b - 1) as u8` patches in the IR
  tier's FP→int NaN fixup: the identical defect, one module over;
* `deopt_stubs.rs` — `u8::try_from(rel)`, which *accepts* 128..=255 and encodes
  them as `-128..=-1`, i.e. a backward branch into the very call the jump exists
  to skip;
* `arith.rs`, `frames.rs` — hand-rolled `(-128..=127)` checks beside a raw cast.
  Correct, but each is a place for the next site to be added without one.

All now go through `ExecutableBuffer::patch_rel8_or_bail` (the body moved down
from `x64::Compiler` so both backends reach it).
`x64::tests::rel8_displacement_patches_all_go_through_the_range_checked_helper`
scans every emitter source and fails on any `try_patch_byte` call that casts its
value; `rel8_patch_out_of_range_bails_instead_of_truncating` additionally pins
the `u8`-vs-`i8` range confusion.

## Reproduce (historical — needs a pre-`7f1b1f263` build)

```bash
cd /data/data/apps/hibernate-orm-harness/hib-suite-runner
CRATONVM_GC=-moving-young <pre-7f1b1f263 cratonvm> --java-home <jdk25> @common.args \
  -Dcraton.batch=1 CratonRunner <listfile-with-ZonedDateTimeTest> 0
```

~30 s to the SIGILL. The Windows-side repro in the superseded document
(`DateSymbolsProbe`, `apps/tomcat-suite-runner/probes/`) reaches fault 1 in ten
seconds and needs no harness.

## What this cost, and the cheap check that would have caught it

Two documents, two crash faces, five hours apart, one encoding bug. The thing
that would have collapsed them into one was available at the fault and was not
taken: **the faulting instruction is not the interesting one — the branch that
reached it is.** `x/40i $pc-0x40` shows the `JNE` and its literal encoded
displacement, and *two bytes* at a `JNE` means `rel8`, which means an
arithmetic check that takes ten seconds. This document instead reasoned from
"some computed address is the true boundary +1" to a list of metadata
producers, none of which was involved.
