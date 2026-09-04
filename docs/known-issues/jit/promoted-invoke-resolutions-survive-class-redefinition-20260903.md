# `invalidate_promoted_for_class` has no production caller: promoted invoke resolutions survive a class redefinition until the next GC

**Status:** OPEN, and **INFERRED, NOT REPRODUCED.** Everything below is the
call graph plus two end-to-end facts; no test demonstrates a wrong dispatch
yet, and the "How to reproduce" section says what such a test needs. That
distinction is the point — the last known-issue page in this family was
right about its bug and wrong about three surrounding claims, each time by
inferring absence from a search that could not have found the thing.

## What is measured

* `vm/src/runtime/lockfree_resolve.rs:458` declares
  `invalidate_promoted_for_class(class_id)`, documented verbatim as
  *"Clear promoted invoke entries whose caller class or receiver class
  matches `class_id` — used by CHA invalidation / class redefinition."*
* It has **no production caller.** Its only references are two call sites
  inside this module's own `#[cfg(test)]` block (lines 613, 791). It is
  offender `fn invalidate_promoted_for_class prod=1 test=2` on
  `vm/tests/no_test_only_public_api.rs`'s list.
* The wholesale sibling `invalidate_promoted()` **does** have exactly one
  production caller: `vm/src/memory/gc.rs:153`, on collection.
* `redefineClasses` / `redefineClasses0` / `redefineClass` are registered
  natives (`vm/src/runtime/instrument.rs:2141, 2365, 2445`), so the path is
  reachable from Java through `java.lang.instrument`.
* The redefinition and CHA paths in `vm/src/vm/vm_exec.rs` invalidate **JIT**
  state and say so ("JIT: fully invalidated N method(s) due to
  redefineClass", "invalidated N method(s) via CHA listener for class"), and
  `grep shared_resolution vm/src/vm/vm_exec.rs` finds **nothing** — the only
  mention anywhere outside `lockfree_resolve.rs` is the constructor in
  `vm_init.rs:4249`.
* The cache being discussed is populated in production: `promoted_invokes`
  is written through `insert_promoted_invoke` from the interpreter's invoke
  paths (module doc, `lockfree_resolve.rs:8-10`).

## What that implies, and what it does not

A class redefinition invalidates compiled code but leaves promoted invoke
resolutions for the affected class in the cache. They are cleared only by
the next garbage collection, which clears **all** of them for an unrelated
reason. So there is a window — one whose length is "until the next GC",
which on a small heap is short and on a large one may be very long — in
which an `invokevirtual`/`invokeinterface` whose target changed under
redefinition could still be served the pre-redefinition target from the
promoted cache.

**Not established:** that any such dispatch actually goes wrong. The
promoted key is `(caller, ?, ?, receiver)` and there may be a validity
check on use that this reading missed; the interpreter may re-verify the
target before dispatching. Somebody should look at `get_promoted_invoke`'s
consumers before treating this as a live defect.

## How to reproduce (what a real test needs)

Not attempted here. It needs:

1. a Java agent using `java.lang.instrument.Instrumentation.redefineClasses`
   (the natives are registered, so this should be reachable);
2. a call site executed enough times to promote its resolution into
   `promoted_invokes` — check with a probe on `insert_promoted_invoke`
   rather than assuming a threshold;
3. a redefinition that CHANGES the method the site should reach;
4. no GC between (2) and (3), or the wholesale `invalidate_promoted()` on
   collection masks the whole thing. **Assert this, do not assume it**:
   `CRATONVM_DBG_ALTRACE=1` prints one line per collection, and a workload
   that allocates little may run the whole test without collecting.

If the post-redefinition call reaches the old target, the fix is one line:
call `invalidate_promoted_for_class(class_id)` from the same place
`vm_exec.rs` invalidates JIT methods, which also gives that function its
production caller back and takes the `no_test_only_public_api` baseline
from 301 to 300.

## How it was found

Triaging the 301 entries on the test-only-public-API ratchet, filtered to
names suggesting a cleanup path (`release|drain|evict|flush|unregister|
clear|invalidate|...`). Eleven matched; most are JVMTI entry points, which
are legitimately reachable only through an agent.

Worth recording about that triage: **the ratchet's list is mostly benign
and is not a backlog of bugs.** Three entries that looked like live
features — `line_number_for_bci`, `NpeMessageGenerator`, and
`create_virtual_thread` — were probed end-to-end against HotSpot and all
three behave identically (stack-trace line numbers, JEP 358 helpful NPE
messages, and `Thread.ofVirtual()` all work). 270 of the 301 are `fn` and
most are test helpers or alternate entry points. The ratchet's value is
the **delta**: `release_submission` was caught because the count ROSE from
318 to 319, not by reading the list.
