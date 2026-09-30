# G1's remark enqueues a phantom reference without nulling its referent slot, then cleanup frees the referent

*Filed 2026-09-26 by gen round 5 wave 1, lane `refs5`, found while writing the
generational twin. G1 is out of this round's scope: filed for the G1 owner. Not
fixed.*

- **Status:** OPEN (G1 owner).
- **Severity:** memory-safety class (a live object holds a dangling pointer); latent.
  How often it bites depends on who reads the slot afterwards (see Consequence).
- **Code:** `vm/src/runtime/interpreter/gc_and_alloc.rs::g1_remark_process_references`.

## What is wrong

Under INT-8, G1 hides every registered `Reference`'s slot 0 from its concurrent
trace. At the final remark, `process_references_with_finalizer_trace` enqueues a
phantom whose referent is unmarked (`process_phantom_refs` sets `enqueued`).
`g1_remark_process_references` then does only two things with the result:

- it nulls slot 0 of the rows `take_newly_cleared` returns, which covers **soft
  and weak only** (`reference.rs::take_newly_cleared` walks `soft_refs` and
  `weak_refs`);
- it links the phantom into its queue.

Nothing nulls the PHANTOM's slot 0. Cleanup then frees the referent's region in
place. The `PhantomReference` itself is live: its queue holds it, and a
`jdk.internal.ref.Cleaner` stays on its class's static list until `clean()`. So
it keeps a slot 0 that names freed memory.

The stop-the-world paths never have this: their pre-collection pass nulls every
active phantom's slot and never restores an ENQUEUED one
(`weak_phantom_active_pairs` excludes it). JDK 9+ semantics agree: a phantom
reference is cleared as it is enqueued.

## Consequence

Any later reader of that slot dereferences a stale address:

- a full (STW) collection tracing the `Reference` marks whatever now lives there,
  or a non-object address inside a reused region;
- `refersTo` on the phantom compares against a stale address (a false `true` is
  possible after the address is reused);
- a young pause's card or remembered-set scan of the old `Reference`.

The next concurrent cycle hides the slot again, so the trace itself is not the
reader.

## Fix (S)

After the enqueue loop in `g1_remark_process_references`, null slot 0 (with
`set_field_suppress_satb`) of every phantom row this round enqueued whose
`Reference` is marked. The generational twin does this in
`gen_remark_process_references` (gen r5w1/refs5) by diffing
`ReferenceProcessor::active_reference_object_set()` before and after the round.
G1 can do the same, or the processor can return the enqueued phantoms in
`ReferenceProcessingResult`, which is cheaper; see
`../../internal/gc/gengc-r5w1-refs5-proposal-processor-reports-what-it-retired-DONE-20260928.md`.

## How to verify

A G1 unit test in `g1.rs`: register a phantom to an old-region object, drop the
object, run a concurrent cycle through `g1_final_remark_cleanup`. Then assert that
the phantom's slot 0 reads `null` and that the referent's region was freed.
