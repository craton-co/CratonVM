# Brooks read-barrier vs CompactHeader: `load_and_forward` reads the wrong forwarding slot

**Status:** OPEN. Needs a maintainer decision (which header format the live STW collector uses).

**Failing test:** `cargo test -p cratonvm-vm --test tier1_tests t1_brooks_barrier_follows_forwarding_pointer`

## Symptom
The test installs a forwarding pointer on `old` and expects `heap.load_and_forward(old)` to
return `new_obj`; it returns the wrong pointer (assertion `forwarded.as_ptr() == new_obj.as_ptr()`
fails).

## Root cause (the inconsistency)
The test and the read-barrier disagree on **where** an object's forwarding pointer lives:

- The **test** (`vm/tests/tier1_tests.rs`) installs forwarding via
  `cratonvm_gc::compact_header::CompactHeader` — i.e. encoded in the **8-byte word at offset 0**
  (the mark word). Its comment claims *"the CompactHeader API is the path stop-the-world GC uses
  during evacuation."*
- The **read-barrier** `gc/src/vm_heap.rs::load_and_forward` reads the **legacy `ObjectHeader`**
  and uses `is_forwarded()` / `forwarding_address()`, whose doc-comment **explicitly** states the
  current backend keeps the forwarding pointer in the legacy `forwarding_ptr` field (offset 24)
  and warns against decoding the compact 8-byte form.

`ObjectHeader` carries **both** a `mark_word` (offset 0) and a separate `forwarding_ptr`
(offset 24), so the two APIs can disagree silently. The test writes offset 0; the barrier reads
offset 24 → mismatch.

These two in-code comments directly contradict each other; one is stale.

## Resolution needed (do NOT guess)
Determine what the **live** collector (`gc/src/gen_heap.rs::forward_object`) actually writes when
it evacuates an object:
- If it writes the **legacy `forwarding_ptr`** (offset 24): the **test** is wrong — fix the test
  to install forwarding through `ObjectHeader`, not `CompactHeader` (safe, test-only).
- If it writes the **CompactHeader** (offset 0): the **read-barrier** is wrong — change
  `load_and_forward` to decode the compact header (a real, high-risk GC fix).

## Risk
Changing `load_and_forward` (the GC read barrier) on a wrong assumption can cause use-after-free /
heap corruption. This must be resolved by confirming the collector's actual forwarding mechanism
before any change — hence deferred.
