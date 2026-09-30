# Proposal: a thread's live JNI local referents join the young pin ledger instead of refusing the pinned copy

> **STATUS (2026-09-28, gc defects round, proposal triage d8/y): KEEP (rank 32
> of 54).** Not built. Engagement of the pinned copy under JNI; matters only
> once `CRATONVM_GEN_PINNED_YOUNG_COPY` is on (NOT YET). **Gate:** the page's
> unit test and `Gcd1JniRootsProbe` arm A with the pinned copy on;
> `pinned_cycles` up on a JNI-heavy run. **Size:** M.

*Filed 2026-09-28 by gcd d4/m (lane young4). A direction for triage, not a
defect.*

- **Status:** PROPOSAL.
- **Backend:** Generational (the young pin ledger).
- **Owners:** `vm/src/native/jni.rs` (lane k: its `for_each_live_local_ref`
  and `local_refs_may_be_held_raw`, landed at `2e2caa0b4`),
  `vm/src/jit/conservative_roots.rs` and the safepoint / blocking deposits
  (who calls them), `gc/src/gc_quiescence.rs` (the ledger row).

## Where things stand after gcd d4/m

While any thread of a VM holds raw JNI local references, the pause's young
pin ledger reads incomplete (`gc_quiescence::raw_jni_locals_open`, fed by
lane k's dispatch). The pinned copy, its take-over arm and option B then fall
back to the sweep. That is sound and never widens `common-w2c`'s exposure,
but a JNI-heavy JIT-warm program loses the pinned copy for every pause that
overlaps a native call.

## Proposed change

The pin a raw local needs is its REFERENT staying where it is. The VM knows
every live local (the thread's JNI local frames), and lane k's iterator
visits them for the calling thread. So:

1. At each thread's own deposit for a pause, in the ledger row this
   proposal adds (per pause, stamped like the peer-register words), note
   the young referent address of every live local, raw.
   - A parked thread deposits at its park.
   - The initiator deposits in its root gathering.
   - A blocked thread would need the standing-deposit proposal's shape
     (`gcd-d3m-proposal-blocked-deposits-stand-in-the-young-pin-ledger-20260927.md`),
     because it deposits before the pause opens.
   - A thread with no JIT entry would deposit this row too; today it
     deposits nothing.
2. The plan pins those pages. Every object a raw local names then stays
   put, and the count-based refusal can go.
3. What it cannot cover, and must keep refusing on: a raw local the native
   stored OFF its frames and past its frame's close (undefined behaviour
   under the JNI spec, but real libraries do it), and a foreign-attached
   thread's attach-level locals (no deposit point). Keep
   `RawJniLocalsScope` for those two, counted by lane k.

## How to verify once built

- Unit: a thread with an open JNI frame holding a young local deposits it,
  the ledger reads complete, and the plan pins its page.
- Runtime: `Gcd1JniRootsProbe` arm A with `CRATONVM_GEN_PINNED_YOUNG_COPY=1`
  prints at least the flag-off arm's PASS lines. `[GC] young_pinned_copy:`
  shows `pinned_cycles` rising on a JNI-heavy run (e.g. the netty-tcnative
  suite) against the count-refusal build.
