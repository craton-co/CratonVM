# `TestOpenClose` — `Exception in thread "main" java/lang/Object` — FIXED 2026-08-30

Retires `known-issues/h2/bug-h2-testopenclose-throwable-is-java-lang-object-20260829.md`.

## What it was

**The throwable was collected out from under the launcher.** Nothing threw a
`java.lang.Object`; the object was a perfectly ordinary exception that no longer
existed by the time it was rendered.

`vm-cli/src/main.rs` gets an `ObjectRef` out of
`MethodCallFailed::ExceptionThrown`, and then — deliberately, because HotSpot
runs hooks on the uncaught path too — runs **shutdown hooks before rendering
it**:

```rust
run_shutdown_hooks(&mut ctx, trigger);          // arbitrary Java: allocates, can collect
match result {
    Err(MethodCallFailed::ExceptionThrown(exc_ref)) => {
        let cid = vm.shared.mem.heap.class_id_of(cur);   // reads a corpse
```

Between those two lines the throwable lives **only in a Rust local**. It is
reachable from nothing the collector scans, so a collection inside a hook
reclaims it. The render then reads a zeroed header, and `ClassId(0)` **is**
`java.lang.Object` — the first class this VM loads.

That also explains the two things the old page called "the whole difficulty":
no message and no frames. The object was not mis-typed, it was **gone**.

H2 supplies the window itself. Its hook is
`org.h2.engine.OnExitDatabaseCloser.run`, which **closes a database** from
inside the hook — as much allocation as the window could want.

## How it was found, and what that rules out

The old page offered two families: something that is not a `Throwable` reached
the throw path, or a receiver's class id decoded as `0`. It is the second — but
not from a stale *receiver*, from a **missing root**.

`CRATONVM_DBG_ATHROW=1` settles it in one run, and the launcher's own output had
been recommending that flag all along:

* **No `ATHROW class=java/lang/Object` anywhere.** Every throw in the run has a
  real class: 24 `DbException`, 24 `InterruptedException`, 23 `MVStoreException`,
  5 `OutOfMemoryError`, and a handful of others.
* The **last** throw before the failure is a genuine
  `org/h2/message/DbException` ("IO Exception: Closing"), with a full stack:

  ```
  org/h2/engine/OnExitDatabaseCloser.run          <- a SHUTDOWN HOOK
  org/h2/engine/OnExitDatabaseCloser.onShutdown
  org/h2/engine/Database.onShutdown
  org/h2/engine/Database.close
  org/h2/engine/Database.closeImpl
  org/h2/engine/Database.closeOpenFilesAndUnlock
  ```

* Nine lines later: `Exception in thread "main" java/lang/Object`, immediately
  after `shutdown hooks: ran=1 threw=0 ... trigger=uncaught`.

`trigger=uncaught` is the confirmation that the hooks were started **by the very
exception being rendered**, so the window is not hypothetical — it is on the
failing path by construction.

## The fix

`JvmThread::uncaught_exception_pending`, wired into **both halves** the tree
requires for a per-thread object slot:

* the scan — `memory/roots.rs` §10, beside `java_thread_obj`,
  `pending_async_exception` and `jit_pending_exception`;
* the remap — `memory/gc.rs::remap_thread_object_slots`.

The launcher parks the throwable there before running hooks and takes it back
afterwards, so it survives the collection **and** the launcher gets its
post-move address.

This is the same defect `jit_pending_exception` had one level down, and the fix
is deliberately the same shape: that slot spent its life in a `thread_local!`
`Cell` where neither half could reach it (`fixed-bugs/jit-signals-root-gap.md`).
A Rust local in the launcher is the same blind spot with a different name.

`moving_gc_rewrites_every_per_thread_object_slot` — the unit test whose whole
job is to catch a half-wired slot — asserted a hard count of **3**, so a fourth
slot would have sat outside the guard while the guard still passed. It now
covers four and asserts the new slot's post-move address.

## Measured

Same command, same host, before and after. This is a correctness change, not a
timing one, so a cross-binary before/after is the right comparison.

**Before:**
```
Exception in thread "main" java/lang/Object
[cratonvm-cli] (no Java stack frames were captured for this exception)
```

**After:**
```
Exception in thread "main" org/h2/jdbc/JdbcSQLNonTransientException:
  Внутренняя ошибка: "org.h2.mvstore.MVStoreException:
  java.lang.OutOfMemoryError: Java heap space (ByteBuffer.allocate 1048576)"
        at org/h2/test/db/TestOpenClose.main(TestOpenClose.java:46)
        at org/h2/test/db/TestOpenClose.testBackupWithYoungDeadChunks(TestOpenClose.java:152)
        ... ~70 lines of frames and a full Caused by: chain
```

## It reproduces on Windows in ~4.5 minutes

The old page had this as an Azure-only repro at 346–451 s, and could not
reconcile it with an earlier Windows run that timed out as a HANG at the 1500 s
cap. It reproduces locally, deterministically, in about four and a half minutes:

```bash
cd apps/h2database/h2
cratonvm --java-home <jdk25> --Xmx 1g -cp "$(cat cp.txt)" org.h2.test.db.TestOpenClose
```

The test classes are in `temp/`, which is the first entry of `cp.txt` — there is
no `craton-testcp.txt` on Windows, which is what the old page's repro block
asked for.

## What is left on this class — NOT this page

The old page listed three separable failures and said a run that fixes one still
fails. That is exactly what happened: with (1) fixed, the class now fails
legibly on (2), and the trace names it.

1. ~~`Exception in thread "main" java/lang/Object`~~ — **this page, FIXED.**
2. `MVStoreException` wrapping `OutOfMemoryError: Java heap space
   (ByteBuffer.allocate 1048576)` on the MVStore background writer — **still
   fails**, and is now readable rather than hidden behind a corpse.
3. A compaction refusal rate of 16 in 17 collections, which is what lets (2)
   happen. The old page already assigned this away from itself: it belongs with
   the per-cycle coverage proof, and `relocation_skipped_jit` is its counter.

(3) causes (2), so whoever takes the coverage proof should expect this class as
a witness. Note the old page's standing rule for any arm here: **read
`relocation_on_proven_jit` before `rc`** — an arm that does not report it above
zero measured the refusal, not the heap.

## Related

* `fixed-suite-bugs/h2-suite-bugs/bug-h2-testkillprocess-zgc-oom-at-97-percent-free-20260821-FIXED-20260829.md`
  — the page this was split out of.
* `fixed-bugs/jit-signals-root-gap.md` — the same missing-root shape one level
  down, and the precedent this fix follows.
* `known-issues/h2/correctness-issues-consolidated.md` — the index this class
  appears in.
