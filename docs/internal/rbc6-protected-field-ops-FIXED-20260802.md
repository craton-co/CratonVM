# RBC.6 admits `getfield`/`putfield`: javac `synchronized` blocks compile — FIXED

**Status:** ✅ **FIXED 2026-08-02** (`fix/rbc6-precise-field-frames-20260802`).
Split out the same day from
[dateformatsymbols-getproviderinstance-compile-bail](dateformatsymbols-getproviderinstance-compile-bail-FIXED-20260802.md),
where fixing `ldc <Class>` left `rbc6-handler-reads-unsafe-local` as the only
remaining reason the date-formatting path kept hot JDK methods interpreted.

## What the fix turned out to be

**Nothing in the codegen.** The capability the exclusion asked for — "a
precise frame at the top-level field arms" — had been built already, in two
separate commits, and nobody went back to the admission list. The exclusion
outlived its cause by five days:

* `putfield` — `cd451facc` ("a compiled putfield on a null receiver must throw
  NPE") put `emit_precise_null_check_field_store` at the TOP of the top-level
  `0xb5` arm, ahead of every inline/compact/helper sub-path. Inside a protected
  range under `precise_exception_frames` it records a reason-10 frame at the
  trapping bci. The one sub-path it skips is the scalar-replaced store, whose
  "objectref" is a dummy with no receiver behind it and so cannot NPE.
* `getfield` — a null receiver reaches `helpers.getfield` on EVERY sub-path
  (compact-inline, uniform-inline, resolved-helper, unresolved-helper), because
  both receiver checks guarding the inline loads —
  `emit_trusted_oop_receiver_check` and `emit_guarded_getfield_receiver_check`
  — start with a null test that branches to the helper. Every one of those
  calls is followed by `emit_post_invoke_exception_check`, which is exactly
  where the reason-9 frame is built.

So the change is `precise_frame_publishing_opcode` admitting `0xb4`/`0xb5`,
plus a real doc of why, plus the opt-out that lets one binary be A/B'd against
itself.

The one genuine exception is the opt-in RAW inline `getfield`
(`CRATONVM_JIT_INLINE_GETFIELD`), which keeps historical null-reads-as-0
semantics: it neither throws nor publishes. `precise_field_ops_enabled`
withdraws the admission whenever that flag is on, so the two can never be
combined.

### Also: `ldc2_w` was never a throwing opcode

`may_throw_without_precise_frame` matched `0x12..=0x14` as "the ldc family
(String/class resolution can allocate)". `ldc2_w` (0x14) is not in that family
— it can only push a `long` or a `double`. The one form that could run Java is
a `CONSTANT_Dynamic` with a long/double descriptor, and `cp_ldc2w_resolver`
answers `None` for anything that is not a `Long`/`Double` pool entry, which is
a permanent compile bail. An `ldc2_w` that survives to codegen is therefore a
bare constant push that cannot throw, allocate or GC.

Cost of the old grouping: every `long`-arithmetic-inside-`try` method lost its
compile. `Rbc6FieldProbe.getfieldLongHandlerLocal` is the witness — one
`long 1000003L` literal in the protected range was the entire reason it stayed
interpreted while its `int` twin compiled.

## Why this was safe to change

The blast radius is exactly "methods that were refused now compile". The
admission list is only consulted inside `if unsafe_local { … }`, i.e. after
RBC.6 has already decided to refuse the method outright. No method that
compiled before this change compiles differently after it.

## Validation

All A/B done with ONE binary, `CRATONVM_JIT_NO_PRECISE_FIELD_OPS=1` as the
control, so nothing else can be confounded with the change.

**The three JDK targets** (`probes/DateFormatPatternProbe`):

| | result |
|---|---|
| admission ON | `hot_but_stuck_in_interpreter=0`, compile-failures=0 |
| admission OFF | the same 3 methods, `reason=rbc6-handler-reads-unsafe-local` |

i.e. `JRELocaleProviderAdapter.getDateFormatSymbolsProvider`,
`JRELocaleProviderAdapter.getNumberFormatProvider` and
`DecimalFormat.format(J…)` all compile, and nothing on that probe's path is
left uncompiled at all.

**`probes/Rbc6FieldProbe.java`** — the acceptance test the old exclusion cited
against these opcodes. Four of its five methods now compile (the fifth is a
separate backend hole, below) and every value matches HotSpot exactly,
including the `acc=3505302427599075008` checksum. With the control flag set,
all five bail. Spot values on the throwing path, all HotSpot-identical:
`getfieldInt=38`, `getfieldRef=60`, `getfieldLong=5000015`, `putfield=66`,
`twoLocals=105205`.

**`probes/SyncBlockFieldProbe.java`** (new) — the half `Rbc6FieldProbe` does
not cover. javac's synthetic handler is `astore_N; aload_MONITOR; monitorexit;
aload_N; athrow`, and `aload_MONITOR` reads a NON-parameter local. A frame that
hands that back as null either throws over the original exception or never
releases the lock, and a leaked monitor is invisible in a return value — so it
is checked directly. `throwsThroughSyncBlock` compiles (`full-compile … len=4177`)
and, across 25 000 exceptional exits plus 40 000 contended calls from two
threads:

* a second thread still acquires the monitor afterwards (no leak),
* `count=440000` exactly — no lost or double increment,
* the propagated exception is still a `NullPointerException`,
* `acc=55801036056`, identical to HotSpot.

Writing that probe took three attempts, each of which quietly proved nothing;
the shape constraints are documented in its header and are worth reading
before writing the next one:

1. **`static` methods.** As small instance methods the targets were never even
   invocation-counted — `CRATONVM_DBG=jit-method-stats` reported three tracked
   methods for the whole run.
2. **Bodies past the inline cap.** An inlined callee is never compiled as a
   method of its own, so `CRATONVM_DBG_DUMP_JIT=LIST` listed no body for it.
3. **No static fields in the block.** `getstatic`/`putstatic` (0xb2/0xb3) are
   still outside the admitted set, so one static-field access inside the
   protected range brings the RBC.6 bail straight back — masking the thing
   under test.

**Suites:** `cargo test -p cratonvm-jit -p cratonvm-jit-api` green;
`cargo test -p cratonvm-vm --lib` green in both feature configurations
(2378/0 plain, 3894/0 synthetic-jdk); the JIT/exception integration targets
`jit_interp_differential`, `jit_local_exception_handler_tests`,
`jit_arity_5plus`, `jit_category2_params`, `jit_null_receiver_npe`,
`jit_collection_ctor_identity`, `differential`, `intrinsic_diff`,
`jit_cold_new_cp`, `monitor_stress`, `exception_tests`, `exception_edge_tests`
all green.

(Three `tiered::`/`ir_lower::` unit tests each flaked ONCE across repeated
full-suite runs at load average ~29 on 16 cores, a different test each time,
all passing in isolation and on re-run. Same wall-clock-bound flake family
already seen on this host; none of them touch RBC.6.)

## What is still excluded, and why

The admission list is a conjunction: one un-admitted throwing opcode anywhere
in a protected range withholds coverage for the whole method. Still out, each
for a real reason:

* **`athrow` (0xbf)** — its lowering stashes the exception and returns the
  sentinel; it publishes no precise frame. Visible as
  `SyncBlockFieldProbe.caughtThroughSyncBlock`, where an OUTER `try` encloses
  the inner `synchronized` block's synthetic `athrow`.
* **`getstatic`/`putstatic` (0xb2/0xb3)** — a static access can trigger
  `<clinit>`, i.e. arbitrary Java. Whether their lowerings already publish (as
  the field ops turned out to) has not been checked; that is the obvious next
  question for anyone extending this list, and the method is the same: write
  the probe, set the control flag, compare.
* **array loads/stores, `ldc`/`ldc_w`, integer divide, allocation, `checkcast`,
  `invokedynamic`** — see `precise_frame_publishing_opcode`'s doc comment.

## Follow-up this exposed — FIXED 2026-08-03

`Rbc6FieldProbe.getfieldRefHandlerLocal` got PAST RBC.6 and was refused by the
single-pass backend instead:
`compile-bail … backend_attempted=true reason=singlepass-codegen(pc=36,op=0xac)`.
That was a pre-existing hole in handler-body codegen that RBC.6 had been
hiding, not something this change caused. The DCE walk revived dead code at
every branch target, so the ternary inside the handler body came back to life
with no recorded operand stack and underflowed at its own merge. Fixed by
reviving on real reachability instead — see
[singlepass-codegen-refuses-handler-body-merge-FIXED-20260803.md](singlepass-codegen-refuses-handler-body-merge-FIXED-20260803.md),
which also names every `failed`-flag refusal site and makes the switch arms
record their branch-target state.

## Corrections to other docs

`fixed-suite-bugs/tomcat/31-synchronized-code-never-jit-compiled-FIXED.md`
claimed `getfield`/`putfield` "are therefore admitted at the relevant
precise-frame sites instead of being rejected by RBC.6". They were removed
again days later; that claim was stale from 2026-07-28 until today, and is the
reason this looked closed when it was not. A correction note now sits on that
doc.
