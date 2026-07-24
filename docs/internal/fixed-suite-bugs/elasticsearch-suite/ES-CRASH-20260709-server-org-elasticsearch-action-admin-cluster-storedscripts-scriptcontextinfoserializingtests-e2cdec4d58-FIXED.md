# ES CRASH - server org.elasticsearch.action.admin.cluster.storedscripts.ScriptContextInfoSerializingTests - FIXED/retired

Status: fixed / retired

Retired: 2026-07-10

Original status: `docs/known-issues/elasticsearch-suite/ES-CRASH-20260709-server-org-elasticsearch-action-admin-cluster-storedscripts-scriptcontextinfoserializingtests-e2cdec4d58.md`
was opened for a hard VM crash (`rc=139`, 0 tests parsed) in
`org.elasticsearch.action.admin.cluster.storedscripts.ScriptContextInfoSerializingTests`:

```text
java.lang.NoSuchMethodError: java/lang/System$1.findNative(Ljava/lang/ClassLoader;Ljava/lang/String;)J
```

reached via the real JDK's `SymbolLookup.loaderLookup()` lambda ->
`SharedSecrets.getJavaLangAccess()` -> `JavaLangAccess.findNative(ClassLoader, String)` — the
same `findNative`/Panama crash family documented as fixed in
`docs/internal/elasticsearch-suite/ES-FAIL-FAMILY-20260709-system1-findnative-FIXED.md` and
`docs/known-issues/elasticsearch-suite/ES-CRASH-FAMILY-20260709-currentdev-fail-probe-rc139.md`.

This doc's original stderr ALSO showed a second, distinct signal on `testConcurrentEquals`/
`testConcurrentToXContent`, after the `findNative` error:

```text
NoSuchMethodError method="java/lang/Object.contains(Ljava/lang/Object;)Z" caller="java/util/Objects.equals(Ljava/lang/Object;Ljava/l..."
```

`Objects.equals(a,b)` should dispatch to `a.equals(b)`, never to a method named `contains` —
this looked like a real method-dispatch/vtable resolution bug distinct from the `findNative`
crash family, and this retirement pass specifically investigated whether it still reproduces on
current `dev` (see "The `Object.contains` signal" section below).

## Retirement result (crash)

`findNative(ClassLoader, String)J` is already registered with the exact descriptor from the
crash (`jla_find_native` in `../../../../native-builtins/src/shared_secrets_bridge.rs`), on both the
concrete `java/lang/System$1` owner and the `jdk/internal/access/JavaLangAccess` interface
fallback — predates this retirement pass, already present on `dev`.

Rebuilt `dev` @ `5f80719eb11c9c1843fb6baba7c39863c7f3ce75` in a fresh worktree and reran the
original doc's repro command (only the worktree/binary paths, and `-Start` index, changed —
see Validation below): the `NoSuchMethodError` and the crash (`rc=139`) no longer occur. The
process now runs to completion (`rc=1`, all 8 tests parsed, 0 crashes) — confirmed by grepping
both stdout and stderr for `findNative`/`System$1`/`SIGSEGV`/`panicked` with no matches.

## The `Object.contains` signal

Grepped both stdout and stderr of every rerun this session (pre- and post- the executor fix
described below) for `Object.contains` / `Objects.equals` / any `NoSuchMethodError` other than
the retired `findNative` one: **zero matches**. The signal does not reproduce on current `dev`.

Investigated why, rather than taking the absence at face value:

- `testConcurrentEquals` and `testConcurrentToXContent` (the two tests that showed the signal in
  the original doc) both now fail *earlier* in their method body, with
  `java.lang.NullPointerException: Cannot invoke "java.util.concurrent.locks.ReentrantLock.lock()"
  because "mainLock" is null` — thrown from `java.util.concurrent.ThreadPoolExecutor`'s real
  `shutdown()` bytecode (`AbstractWireTestCase.testConcurrentEquals`/`testConcurrentToXContent`
  each spin up `Executors.newFixedThreadPool(n)` and `.shutdown()` it once the concurrent
  equals/toXContent checks finish). This is a newly root-caused, separate bug — see
  [`ES-FAIL-20260710-executors-factory-synthetic-mainlock-npe.md`](../../known-issues/elasticsearch-suite/ES-FAIL-20260710-executors-factory-synthetic-mainlock-npe.md)
  — that currently prevents these two tests from ever reaching the point in their body where the
  original `Object.contains` signal was observed (the actual per-thread `a.equals(b)` concurrent
  comparison loop). So the *current* absence of the `Object.contains` signal is not, by itself,
  proof that the underlying dispatch behavior is fixed — the test simply dies earlier now.
- However, the original doc's own timeline is also informative: the `Object.contains` warning
  fired ~15 seconds *after* the `findNative` `NoSuchMethodError` in the same crashing process,
  and the whole run ultimately died with `rc=139` (`SIGSEGV`) and 0 tests parsed — i.e. the
  `Object.contains` warning was itself downstream of an already-corrupted-and-crashing VM
  instance, in the same run that never got a clean measurement of anything. A WARN-level
  "recovered" `NoSuchMethodError` from the `findNative` bug (Panama `SymbolLookup`/method-handle
  bootstrapping running very early, near VM/class-init) plausibly left nearby interpreter/GC
  state (inline caches, resolution-cache entries) in a partially-populated or corrupted state for
  the remainder of that same process — consistent with the "was itself part of the crash cascade"
  outcome called out as a possibility for this investigation.
- Attempted to disentangle the two by writing a standalone probe
  (`Executors.newFixedThreadPool(4)` + `submit()` + `shutdown()`, no ES/JUnit involved) to see
  whether the executor bug is itself just a symptom that would clear once truly fixed, so the
  equals-dispatch code path could be reached directly. That investigation is what led to
  root-causing and partially fixing the executor bug (see below) — but even after the fix, the
  standalone probe still NPEs (now on `ctl.get()` inside `submit()`/`execute()`, before reaching
  `shutdown()`), because the deeper issue (CratonVM's `Executors.*` factory methods hand back an
  object that LOOKS real — same class name as a genuine `java.util.concurrent.ThreadPoolExecutor`
  — but was never run through the real constructor, so its `ctl`/`workQueue`/`mainLock`/`workers`
  fields are never initialized) is still open. So this session was not able to get execution past
  the executor bug far enough to directly re-exercise the original `equals()` dispatch site.

**Conclusion**: the `Object.contains` signal does not reproduce on current `dev`, most likely
because it was collateral of the same `findNative` crash cascade in the original run rather than
an independent, standing dispatch bug — but this is not proven with a clean, direct
re-exercise of the `a.equals(b)` call site, because a newly-identified, unrelated executor bug
currently makes that code path unreachable in this test. No `../../../../vm/src/runtime/interpreter.rs`
`invokevirtual`/`invokeinterface`/vtable-slot-index change was made in pursuit of this signal —
static review of `../../../../vm/src/runtime/vtable.rs` (`Vtable::lookup_slot`, `Vtable::add_method`) and
`../../../../vm/src/runtime/interpreter.rs` (`execute_invokevirtual_vtable_fast`, `execute_invokevirtual_cached`)
found the receiver-class/name/descriptor verification on all relevant fast paths intact (a
genuine `(name, descriptor)` hash-bucket collision is guarded by an exact-string-match
verification loop in `Vtable::lookup_slot`, and `execute_invokevirtual_cached`'s
`VirtualBytecode` case rejects a cached target when `actual_class_id != receiver_class_id`), so
if the underlying bug is real it is not an obviously-missing guard in those functions — should it
resurface once the executor bug is fixed, it needs its own fresh repro.

## The executor bug found along the way (partially fixed, residual filed separately)

While investigating the above, root-caused why `testConcurrentSerialization`,
`testConcurrentHashCode`, `testConcurrentEquals`, `testConcurrentToXContent` all fail with
`"mainLock" is null`: `Executors.newFixedThreadPool(int)`'s native override in
`../../../../native-builtins/src/phases_early.rs` synthesized an object mistagged as
`java/util/concurrent/ScheduledThreadPoolExecutor` (copy-pasted from the
`newScheduledThreadPool`/`newSingleThreadScheduledExecutor` registrations immediately above it in
the same file, never corrected to plain `java/util/concurrent/ThreadPoolExecutor`). Real JDK
`newFixedThreadPool()` never returns an STPE. The same copy-paste mistake was present in
`newCachedThreadPool()` (both overloads) and `newSingleThreadExecutor()` in the same file.

Fixed the class tag for all four (`../../../../native-builtins/src/phases_early.rs`, the
`register_executor_natives`-equivalent block starting at `let ex = "java/util/concurrent/Executors";`)
to correctly allocate `java/util/concurrent/ThreadPoolExecutor`, matching the sibling (currently
dead/shadowed) registration in `native-builtins/src/lib.rs::native_new_fixed_pool`. This is a
real, narrow, verified correctness fix — confirmed via a standalone probe that
`Executors.newFixedThreadPool(4).getClass()` now correctly reports
`class java.util.concurrent.ThreadPoolExecutor` instead of the wrong
`ScheduledThreadPoolExecutor`, and the resulting stack traces on failure now show a sane,
real call chain (`AbstractExecutorService.submit` -> `ThreadPoolExecutor.execute`) instead of a
nonsensical scheduled-executor one (`submit` -> `schedule` -> `delayedExecute` -> `isShutdown`).

This fix alone does **not** resolve the `"mainLock" is null` / `"ctl" is null` NPEs — a second,
deeper issue (`../../../../native-api/src/registry.rs`'s `drop_real_layout_synthetic` gate assumes every
`java/util/concurrent/ThreadPoolExecutor`-tagged object went through the real constructor, which
is not true for CratonVM's own `Executors.*` factory shortcuts) remains open. See
[`ES-FAIL-20260710-executors-factory-synthetic-mainlock-npe.md`](../../known-issues/elasticsearch-suite/ES-FAIL-20260710-executors-factory-synthetic-mainlock-npe.md)
for the full root cause and suggested fix direction; not attempted in this session because it
requires either constructing these objects via a real `<init>` invocation or adding a
per-object "is this actually real" marker, both larger and riskier changes than this session's
scope warranted given the "don't guess-fix" guidance for exactly this kind of open-ended
architectural gap.

## New residuals found (tracked separately, OPEN)

With the crash gone, `ScriptContextInfoSerializingTests` now runs all 8 of its tests to
completion, but 5 FAIL on two distinct symptoms, both unrelated to the retired crash:

- 4 failures (`testConcurrentSerialization`, `testConcurrentHashCode`, `testConcurrentEquals`,
  `testConcurrentToXContent`) — the executor `"mainLock" is null` NPE described above. See
  [`ES-FAIL-20260710-executors-factory-synthetic-mainlock-npe.md`](../../known-issues/elasticsearch-suite/ES-FAIL-20260710-executors-factory-synthetic-mainlock-npe.md).
- 1 failure (`testFromXContent`) — `org.elasticsearch.xcontent.XContentParseException: [-1:186]
  Invalid shared name reference 26; only got 4 names in buffer (invalid content)` /
  `com.fasterxml.jackson.core.JsonParseException`. Looks like a Jackson smile/shared-string-table
  encoding mismatch unrelated to the retired crash or the executor bug above; not investigated
  further this session.

## Validation

Built `dev` @ `5f80719eb11c9c1843fb6baba7c39863c7f3ce75` in a fresh worktree
(`/data/data/wt-es-storedscripts-retire-20260710`, branch
`fix/es-storedscripts-retire-20260710`), binary copied to
`/data/data/cratonvm-targets/es-storedscripts-retire-20260710/release/cratonvm-es-storedscripts-retire-20260710`
(rebuilt a second time after the `phases_early.rs` executor-tag fix; both builds' repro output
grepped clean of `findNative`/`System$1`/`Object.contains`). Reused the existing built ES
checkout at `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch`
and its sibling `../../../../apps/elasticsearch-suite-runner/run-elasticsearch-suite.ps1` script.

**Index drift note**: see the sibling `GetScriptContextResponseTests` retirement doc for the full
explanation. This session used a freshly generated `-Category all` list (deterministic,
scan-based, no reference-file dependency) and located `ScriptContextInfoSerializingTests` at
index **331** (311 + 20, same 20-class drift as the sibling docs in this retirement pass).

Rerun of the original doc's repro command (paths/index adjusted as above, final rerun after the
executor-tag fix):

```powershell
pwsh -NoProfile -ExecutionPolicy Bypass -File apps/elasticsearch-suite-runner/run-elasticsearch-suite.ps1 -Category all -Jit on -Vm craton -ElasticsearchRoot "<es-checkout>" -WorkDir "<worktree>/apps/elasticsearch-suite-runner/.suite-es-storedscripts-retire-20260710" -Exe /data/data/cratonvm-targets/es-storedscripts-retire-20260710/release/cratonvm-es-storedscripts-retire-20260710 -JdkHome /usr/lib/jvm/java-21-openjdk-amd64 -TimeoutSec 600 -RunName repro-e2cdec4d58-v2 -ModeName repro-e2cdec4d58-v2 -Start 331 -Count 1
```

Result: `FAIL 9.9s org.elasticsearch.action.admin.cluster.storedscripts.ScriptContextInfoSerializingTests`,
`rc=1`, 8/8 tests parsed, 5 failed — no crash, no `NoSuchMethodError` on `System$1.findNative` or
`Object.contains` anywhere in stdout/stderr. This confirms the documented crash is gone; the
remaining 5 failures are the two residuals described above.

Evidence:
- stdout: `.../results/repro-e2cdec4d58-v2/repro-e2cdec4d58-v2/logs/server.org.elasticsearch.action.admin.cluster.storedscripts.Scri.e001f4f1dd59.out.log`
- stderr: `.../results/repro-e2cdec4d58-v2/repro-e2cdec4d58-v2/logs/server.org.elasticsearch.action.admin.cluster.storedscripts.Scri.e001f4f1dd59.err.log`
- results.tsv: `.../results/repro-e2cdec4d58-v2/repro-e2cdec4d58-v2/results.tsv`
  (all under `/data/data/wt-es-storedscripts-retire-20260710/apps/elasticsearch-suite-runner/.suite-es-storedscripts-retire-20260710/`)
