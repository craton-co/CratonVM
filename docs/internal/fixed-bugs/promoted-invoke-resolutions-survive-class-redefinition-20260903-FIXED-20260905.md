# `invalidate_promoted_for_class` has no production caller: promoted invoke resolutions survive a class redefinition until the next GC

**Status:** ✅ **CLOSED 2026-09-05 — the inferred defect does not exist.**
Promoted invoke resolutions do **not** survive a class redefinition. Every
entry is gated per-entry and evicted on its next read. Measured end to end
against HotSpot with a `java.lang.instrument` agent; the function whose
absent caller started this is deleted.

**Retired from** `docs/known-issues/jit/`.

---

## What the original page said, and why it was careful to hedge

It reasoned from the call graph. `invalidate_promoted_for_class(class_id)`
was declared in `vm/src/runtime/lockfree_resolve.rs`, documented as *"used
by CHA invalidation / class redefinition"*, and called by nothing but its
own two `#[cfg(test)]` tests. `redefineClasses0` is a registered native, so
the redefinition path is reachable from Java. `vm_exec.rs` invalidates JIT
state on redefinition and says so in its logs, and `grep shared_resolution
vm/src/vm/vm_exec.rs` finds nothing. From that: a redefinition leaves
promoted invoke resolutions in place until the next GC clears them
wholesale, so there is a window in which a call site could be served the
pre-redefinition target.

Every one of those observations is still true. The page also said, in its
own words:

> **Not established:** that any such dispatch actually goes wrong. […]
> Somebody should look at `get_promoted_invoke`'s consumers before treating
> this as a live defect.

That hedge was right, and the answer was one function away.

## What looking finds

`get_promoted_invoke` does not hand back what it stored. It checks
`CachedInvokeTarget::is_stale()` on **every hit**:

```rust
match hit {
    Some(t) if !t.is_stale() => { … Some(t) }
    Some(_stale) => { /* take the write guard, remove the entry */ None }
    None => None,
}
```

Every variant carries a `RedefineGate`: a `u32` snapshot plus an
`Arc<AtomicU32>` handle to `ClassManager::redefine_generations[class_id]`.
`redefine_class` bumps that counter with `Release` in its step 7;
`is_stale` loads it with `Acquire`. So the eviction is **per-entry, on
use, and immediate** — not "at the next GC", and not a sweep at all.

The class each entry is bound to is the one whose body it caches. The
finding, reproducible without line numbers, which rot:

```bash
grep -n class_redefine_generation_handle \
     vm/src/runtime/interpreter/dispatch_virtual.rs \
     vm/src/runtime/interpreter/dispatch_static.rs
```

Seventeen sites on 2026-09-05. Split by what they bind to:

* **`declaring_id`** (or `entry_cached.declaring_class_id`) — every site that
  caches a class's BYTECODE: `CachedInvokeTarget::Bytecode` in
  `dispatch_static`, `VirtualBytecode` in `dispatch_virtual`, and the
  re-promotion path that copies an existing record's declaring class. This is
  the set the page's window story was about, and it is bound to the right
  class.
* **`receiver_class_id`** — `Intrinsic` and `VirtualNative` only. Those cache
  a VM callback, not a class's bytecode. A redefinition changes bytecode;
  whether a *registered native* should still win at such a site is asked
  separately, at populate time, by `declaring_redefined_not_immune`.
* **`get_loaded_class_id(&class_name)`, falling back to
  `RedefineGate::never_stale()`** — two `dispatch_static` sites, for a native
  or intrinsic whose declaring class the loader cannot name. Also not bytecode.

So no promoted entry serves a class's stale BYTECODE, which is the only thing
a JEP 109 redefinition can change: `redefine_class`'s step-4 structural checks
reject any add, remove, rename, reorder-to-a-different-set or modifier change
of a method or field, and reject a changed superclass or interface list.

## Measured, not inferred

`test_classes/redefine/` (this fix) is the test the original page specified
and did not build: a `premain` agent holding `Instrumentation`, a hot call
site, a redefinition that changes the method body, and a read afterwards.

The second thread is the point. The warm-up thread's own `invoke_cache`
would answer the post-redefinition call without ever consulting the
cross-thread promoted map, so a single-threaded probe cannot see the window
this page was about. A freshly started thread has an empty local cache and
therefore comes through `get_promoted_invoke` on its first dispatch.

Two cases, `--java-home jdk-25`, 200 000 warm-up calls:

* **A** — the redefined class *is* the receiver (`Impl.tag()`).
* **B** — the redefined class *declares* the method and the receiver is a
  subclass that does not override it (`Sub extends Sup`, `Sup.tag()`), so
  declaring ≠ receiver.

```
                      HotSpot     cratonvm
A.warm                OLD         OLD
A.otherThreadPre      OLD         OLD
A.sameThreadPost      NEW         NEW
A.otherThreadPost     NEW         NEW
B.warm                SUP-OLD     SUP-OLD
B.otherThreadPre      SUP-OLD     SUP-OLD
B.sameThreadPost      SUP-NEW     SUP-NEW
B.otherThreadPost     SUP-NEW     SUP-NEW
```

Identical. Case B is the one the "window" story would have failed:
`invalidate_promoted_for_class` could not have saved it either, which is
the next section.

## The proposed one-line fix would not have fixed it

The page's closing suggestion was to call
`invalidate_promoted_for_class(class_id)` from the same place `vm_exec.rs`
invalidates JIT methods. That function retained on the key's **caller** and
**receiver** class ids:

```rust
guard.retain(|(caller, _, _, rcv), _| *caller != class_id && *rcv != Some(class_id));
```

What goes stale under a redefinition is the **declaring** class's body —
a different class from the receiver in every inherited-method case, which
is case B above. So the sweep would have missed exactly the case the gate
handles, while adding a full 16-shard write-lock walk to the redefinition
path to do nothing the gate does not already do.

It is deleted rather than wired. `vm/tests/no_test_only_public_api.rs`'s
`BASELINE_OFFENDERS` drops 301 → 300 — the same movement the page
predicted, by the opposite route.

`invalidate_promoted()`, the wholesale sibling, keeps its one production
caller in `vm/src/memory/gc.rs:153`: class *unloading* can recycle a
`ClassId` and drops its generation counter entirely, so a gate would have
nothing left to compare against. That one is not redundant.

## Guard left behind

`a_redefine_generation_bump_evicts_the_promoted_entry_on_the_next_read` in
`vm/src/runtime/lockfree_resolve.rs`, replacing the deleted function's own
test. It inserts with a real `RedefineGate::snapshot`, bumps the counter as
`redefine_class` step 7 does, and asserts both that the read returns `None`
**and** that `promoted_invoke_count()` has gone to zero. The second
assertion is the one that earns its place: returning `None` alone would
leave the stale entry for every sibling thread to rediscover.

## The methodological note the original page opened with, kept

It opened by saying it was *inferred, not reproduced*, and that the last
page in this family "was right about its bug and wrong about three
surrounding claims, each time by inferring absence from a search that could
not have found the thing". This page then did the same thing one level up:
it inferred a *defect* from the absence of a *caller*, having established
that it had not checked the consumer that would decide. Labelling the
inference honestly is what made it cheap to close — the falsifier was
written into the page, and running it took one probe.

The ratchet triage note it also carried is unchanged and still worth
keeping: **the test-only-public-API list is mostly benign and is not a
backlog of bugs.** Its value is the *delta*. This entry is one more
confirmation — the offender was real, and the reason it was an offender was
that the function should not exist, not that a caller was missing.
