# JIT round 14 wave 5, lane review5: proposals

Ranked. Filed while reviewing the wave-4 commit (587fe553d); see
`r14w5-review5-wave4-review-findings-FIXED-20260929.md`.

## RV5-1. SeqCst interrupt handshake for the Object.wait wake

- Benefit: closes the formal store-buffering window of finding 1b (interrupter reads a
  thin mark before the waiter's inflation; the enrolment re-check must see the flag).
  With MON14-4's 1 s safety slice a miss there costs a second.
- Cost: three orderings: `thread_registry.rs` `set_interrupted` store `SeqCst`;
  `monitor.rs` `MonitorTable::wake_waiter_for_interrupt` `fence(SeqCst)` before the mark
  load; `Monitor::wait`'s enrolment re-check load `SeqCst`. Interrupts are cold.
- Risk: none functional; `vm/src/threading/**` is interpreter-round territory for
  `jvm_thread.rs` only, the registry is not.
- First step: the three edits, plus a unit test that interrupts a waiter whose monitor is
  still thin when the interrupter looks (drive `wait` with an un-inflated object).

## RV5-2. Interrupt wake keyed by the waiting thread, not the registry slot

- Benefit: the wake no longer depends on the registry's waiting-monitor slot, the mark
  being inflated, the monitor index, or the Java-tid-to-`ThreadId` resolution agreeing
  with the waiting `JvmThread` (finding 1a, virtual threads). The waiter publishes its
  `Arc<Monitor>` (or its own condvar) in a per-thread cell before enrolling; the
  interrupter signals that directly.
- Cost: one per-thread cell (a `JvmThread` field shared through the registry entry, as
  the interrupted flag already is), written before enrolment and cleared after leaving
  the wait set.
- Risk: medium: lifetime of the published `Arc` across monitor deflation; keep the
  current path as fallback behind a switch.
- First step: measure with `R14Review5WaitInterruptLatency` whether 1a is real.

## RV5-3. Fold the inlined `String.length()` / `isEmpty()` body on a literal

- Benefit: the literal fold (C14W3-4) folds only the call and the intrinsic's `UShr`
  shape. When `String.length()` is SPLICED (intrinsics off, or inside a splice where the
  expansion refuses), its body is `value.length >> coder()`: an `Shr` over
  `ArrayLength(Load<Ref>)` and a byte `coder` read (through `coder()`'s
  `COMPACT_STRINGS` test). Matching that shape on a `ConstString` receiver makes the
  fold independent of which route built it.
- Cost: one more arm in `ir_fold_string_literal_queries`; the exact node shape must be
  read off a real splice dump (`CRATONVM_DBG_IR_COMPILES`).
- Risk: low if matched as strictly as the `UShr` arm (same receiver node on both loads).
- First step: dump the graph of `R14Calls2LiteralQuery`'s `spliced` phase under
  `CRATONVM_JIT_IR_STRING_INTRINSICS=0` and record the shape.
