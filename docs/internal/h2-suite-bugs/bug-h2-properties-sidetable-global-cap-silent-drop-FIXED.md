# `java.util.Properties` side-table's global 10,000-object cap silently drops writes for every `Properties` instance created afterward — process-lifetime, not concurrent-count

## Status
**FIXED** — 2026-07-22, `dev` (see commit referenced in the merge that landed
this doc move). Originally opened 2026-07-21 as
`docs/known-issues/h2-suite-bugs/bug-h2-properties-sidetable-global-cap-silent-drop.md`.

## Severity (as filed)
**HIGH** — silent data loss with no exception, in one of the most
fundamental JDK classes.

## Affected test classes (as filed) — see "Scope correction" below
- `org.h2.test.db.TestAnalyzeTableTx` — **confirmed fixed**, full class run PASS.
- `org.h2.test.synth.TestThreads` — **confirmed fixed**, full class run PASS.
- `org.h2.test.jdbcx.TestConnectionPool` / `org.h2.test.jdbcx.TestDataSource` —
  **NOT caused by this bug** (misattributed in the original filing — both
  classes surface the identical H2 exception text, which is exactly the trap
  the original doc's own "How this differs from the JAAS finding" section
  warned about, just missed for these two). See "Scope correction" below and
  the new doc filed for their actual cause.

## Root cause (unchanged from original filing)
`native-builtins/src/properties_sidetable.rs` implements `java.util.Properties`
storage via a process-wide side-table keyed by GC-stable object identity. It
enforced `MAX_TOTAL_OBJECTS = 10_000` as a **hard, never-decremented**
ceiling: once 10,000 distinct `Properties` objects had ever been registered
in the process's lifetime, every subsequent brand-new object silently lost
all `put`/`getProperty` calls forever, with no exception and no eviction —
`MAX_TOTAL_OBJECTS` was a cap on objects *ever constructed*, not objects
*concurrently alive*, despite its own doc comment claiming the latter.

## Fix (native-builtins/src/properties_sidetable.rs)
Replaced the hard ceiling with real GC-aware lifecycle tracking, so the cap
now means what it always claimed to mean — concurrently *alive* tracked
objects, not objects ever constructed:

1. **Weak-reference tracking**: every newly-registered `Properties` object
   gets a real `java.lang.ref.WeakReference` (constructed via
   `ctx.new_object_initialized`, which dispatches to the VM's own
   `native_weak_ref_init_queue` — the same machinery real `WeakReference`
   users get) enqueued on a single process-wide `java.lang.ref.ReferenceQueue`.
   The `WeakReference` itself is kept alive via `ctx.add_global_root` (a real
   GC root) so it survives long enough to be discovered and enqueued when its
   referent (the `Properties` object) becomes unreachable; the referent stays
   only weakly reachable, so it can still be collected normally.
2. **Opportunistic + forced reclaim**: hitting the cap now first drains
   whatever the GC has already found unreachable (`drain_reclaimed` — cheap
   `ReferenceQueue.poll()` calls), and if that's not enough (e.g. a tight
   allocation loop that hasn't triggered a collection yet), forces a GC cycle
   (`ctx.force_gc()`) and drains again before falling back to the old
   silent-drop behavior — which should now essentially never trigger for real
   workloads, only for a genuine, simultaneous 10,000-concurrently-alive-object
   pathological case.
3. **GC-move safety**: the `Properties` object being registered is pinned
   (`ctx.pin_native_root`/`read_native_pin`) across the reclaim/force-GC calls
   (which can relocate it under the moving collector) before being handed to
   `WeakReference`'s constructor, and the shared `ReferenceQueue`/each
   `WeakReference` is re-resolved via `ctx.resolve_global_root` on every use
   rather than cached across calls that can themselves trigger GC.
4. Best-effort throughout: contexts that don't support real GC integration
   (test mocks) simply never get eviction — identical to the pre-fix
   behavior, no regression there.

## Scope correction — TestConnectionPool / TestDataSource are a different bug
The original filing listed all four test classes as affected purely because
they share the exact same downstream H2 exception text
(`JdbcSQLInvalidAuthorizationSpecException: Wrong user name or password`).
Investigating them against the fix above showed they do **not** share this
root cause:
- The fixed binary's `MAX_TOTAL_OBJECTS` cap is never even approached during
  either class's run (confirmed via temporary instrumentation — the
  side-table stays orders of magnitude under 10,000 tracked objects the
  entire time).
- Both classes fail identically with or without this fix, and identically
  with `--nojit` (interpreter-only) — ruling out both the cap and any
  JIT-tier-up involvement.
- The unfixed baseline binary fails at the exact same point.
- `Properties.put("user", ...)`/`put("password", ...)` on the failing
  connection's info object (and H2's own internal `ConnectionInfo` copy)
  **do** succeed via the side-table (`is_new=false`, i.e. correctly
  re-targeting an already-tracked object) immediately before
  `Engine.validateUserAndPassword` reports empty/wrong credentials — a
  different failure shape than this doc's mechanism (which was a pure
  registration-time reject, not a post-write read gap).
- Confirmed real HotSpot JDK25 passes both classes cleanly (matches the
  original filing's own claim), so this is a genuine CratonVM-specific bug —
  just not this one.

This has been re-filed as a new, separate OPEN issue:
`docs/known-issues/h2-suite-bugs/bug-h2-connectionpool-datasource-wrong-password-not-cap.md`.

## Verification
- Standalone repro from the original filing (10,001× `new Properties();
  p.put("user","SA"); p.put("password","sa"); p.getProperty(...)`in a tight
  loop): confirmed failing on the unfixed baseline (`firstFail=10000`,
  matching the original filing exactly) and passing cleanly (0 failures)
  on the fixed binary.
- `org.h2.test.db.TestAnalyzeTableTx` (real class, full run, 10,000-connection
  loop): PASS on the fixed binary (was the primary confirmed repro for this
  bug).
- `org.h2.test.synth.TestThreads` (real class, full run): PASS on the fixed
  binary.
- Full 218-class H2 suite regression pass (`jit-real`, real JDK25 backend) —
  see `apps/h2database-suite-runner/out/propscap-fullregress-20260722-*` —
  no new failures relative to the prior known-baseline categorization; only
  pre-existing, independently-tracked failures remain (`TestAuthentication`
  / JAAS, `TestConnectionPool`/`TestDataSource` per the scope correction
  above, etc).
- Built with `CARGO_PROFILE_RELEASE_LTO=off` on the Azure Linux host in an
  isolated worktree/branch (`fix/h2-properties-sidetable-global-cap-20260722`),
  not the shared main checkout.
