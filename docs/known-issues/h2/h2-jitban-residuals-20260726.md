# Open residuals blocking the `org/h2/` JIT ban (HIB-LONGTAIL.1) — 3 classes, plus one performance wall

**Status:** OPEN. This is the hand-off doc for the two H2 investigations that
closed on 2026-07-26; both of those are archived under
`docs/internal/fixed-suite-bugs/h2-suite-bugs/` and neither needs re-reading to
act on what is below.

* [`h2-jitban-schema-not-found-on-reconnect-FIXED.md`](../../internal/fixed-suite-bugs/h2-suite-bugs/h2-jitban-schema-not-found-on-reconnect-FIXED.md)
  — the systemic `Schema  not found` metadata corruption behind the ban. Fixed,
  extinct (0/218 classes).
* [`bug-h2-testfilesystem-testconcurrent-async-hang-FIXED.md`](../../internal/fixed-suite-bugs/h2-suite-bugs/bug-h2-testfilesystem-testconcurrent-async-hang-FIXED.md)
  — seven per-invoke costs, the last three fixed the same day. The class still
  misses the 300s watchdog; see "Residual 4" below for what that actually is.

## Where the ban stands

`HIB-LONGTAIL.1` (`vm/src/jit/skip_list.rs`) still bans `org/h2/`. The reason
has now changed twice: it was written as a throughput safety net (stale — the
Hibernate longtail it cites was root-caused to the executor bridge on
2026-07-15), then re-justified as a systemic metadata corruption (fixed), and
is now down to **three classes**.

Same-binary A/B over the ten classes that regressed on 2026-07-26, rerun on
`dev` after that day's four JIT fixes:

| Class | ban in place | ban lifted | verdict |
|---|---|---|---|
| `org.h2.test.unit.TestReopen` | PASS | **PASS** | closed |
| `org.h2.test.store.TestObjectDataType` | PASS | **PASS** | closed — array `instanceof` |
| `org.h2.test.unit.TestUpgrade` | PASS | **PASS** | closed — invokespecial loader |
| `org.h2.test.mvcc.TestMvccMultiThreaded` | PASS | **PASS** | closed |
| `org.h2.test.synth.TestKillRestart` | PASS | **PASS** | closed |
| `org.h2.test.unit.TestCache` | PASS | **PASS** | closed |
| `org.h2.test.db.TestCompatibility` | FAIL | HANG | **no longer ban-attributable** — fails BOTH ways |
| `org.h2.test.store.TestStreamStore` | PASS | FAIL | **residual 1** |
| `org.h2.test.store.TestFreeSpace` | PASS | HANG | **residual 2** |
| `org.h2.test.synth.TestNestedJoins` | PASS | HANG | **residual 3** |

Six of ten closed. Two of those six were closed by named fixes (below); the
other four were closed by the combination of that day's JIT work and what
landed on `dev` alongside it — they were not bisected individually, so do not
cite a specific commit for them.

The two named fixes, both general x64 JIT defects that H2 only exposed:

* **`373e780b7` — an array is not an instance of its component type.**
  `jit_typecheck_resolve` consulted the array-descriptor rule but honoured only
  a positive answer; a negative one fell through to a hierarchy comparison
  against `obj_class_id`, which for a reference array is the header's
  *component* class id. `String[] instanceof String` was therefore true, as was
  `Integer[] instanceof Integer` and (via the subclass walk)
  `String[] instanceof CharSequence`. Regression:
  `regression-suite/src/RJitArrayTypecheck.java`.
* **`f16acca12` — invokespecial resolved by name, ignoring the caller's
  loader.** `jit_invoke_dispatch` handed raw constant-pool text to
  `invoke_special_shared`, which resolves through the global binary-name map;
  with two loaders defining `org/h2/util/IntArray` it picked the wrong one.
  `JitInvokeInfo` now carries `declaring_class_id` and the arm resolves through
  `resolve_class_loader_aware`.

## Residual 1 — `TestStreamStore`: `Interruptible.interrupt` NPE, INTERMITTENT

```
org.h2.mvstore.MVStoreException: java.lang.NullPointerException:
  Cannot invoke "sun.nio.ch.Interruptible.interrupt(java.lang.Thread)"
  because "this.interruptor" is null   [2.4.249/3]
  … org.h2.mvstore.FileStore.storeBuffer → MVStore.panic → newMVStoreException
```

`interruptor` is `AbstractInterruptibleChannel`/`AbstractSelector`'s field,
assigned in `begin()` immediately before `interruptor.interrupt(me)` reads it
back — the classic "the field write did not stick by the time the next read
happens" shape, in `java.nio`, not in H2.

**Read this before assuming it is a ban regression.** The A/B above shows
PASS-with-ban / FAIL-lifted, but a standalone run of this same class WITH the
ban in place, on the same binary, also produced this NPE. It is intermittent.
Before spending time on the JIT, run it ~10x in each configuration and get a
failure rate; if it fails with the ban too, it is a concurrency bug that the
ban's slower execution merely hides, and it belongs in its own doc rather than
this one. The doc it originally came from listed this exact signature as an
already-suspected residual.

## Residuals 2 and 3 — `TestFreeSpace`, `TestNestedJoins`: 300s timeout, no exception

Both hang cleanly (no stack, no output) with the ban lifted and pass with it in
place. Nothing is known about them beyond that.

They need the treatment that worked for `TestFileSystem`: a timeout-free run
with per-sub-test timing, because the suite runner's 300s cap tells you nothing
about whether this is a livelock or merely slow. With `org/h2/` JIT-eligible,
H2 code is throughput-competitive, so a 300s HANG here is more likely a lost
wakeup or a livelock than slowness — the opposite of the prior on this suite.
Method: copy the one test class out of the shared H2 checkout, add a per-phase
timer, compile it into a private directory and prepend that to `-c` (see the
`classpath-overlay-instrumentation-technique` memory). Then bisect with
`CRATONVM_JIT_BISECT_ONLY` / `CRATONVM_JIT_BISECT_SKIP` exactly as the two
closed fixes were.

## Residual 4 — `TestFileSystem.testConcurrent` on `nioMemLZF:1:`

Carried over from the archived TestFileSystem doc, which has the full history.
The short version, because the doc's own title is misleading:

* The class is **not** uniformly slow. It clears ~14 of its ~16 filesystem
  prefixes in ~130s, then parks in `testConcurrent` on `nioMemLZF:1:` for
  **>18 minutes without completing**. HotSpot does that sub-test in 862ms and
  the whole class in 4.94s.
* `async:` — the prefix in the old doc's filename — is the **least** affected
  at 4x. The name is a misnomer.
* The amplifier is a backoff-free `AtomicIntegerArray` spin lock
  (`while (!locks.compareAndSet(pos, 0, 1)) {}`) wrapped around an
  interpreted-speed critical section. `compareAndSet` + `set` measures 535ns
  per pair against HotSpot's 20ns.

**The one concrete lead**: `invoke_or_native` probes the native registry on
every call, even though `resolve_method_metadata` already caches the resolved
native callback and kind per constant-pool entry
(`ResolvedMethod::native_target` / `native_kind`) — its own doc comment says
"the registry hash is paid once per resolved CP entry, not once per call site
that consumes it". A live profile of the stuck run attributes **12.2%** of both
threads to `NativeMethodRegistry::slot_for_exact` plus 9.3% to
`invoke_on_class_shared_inner`. Threading the precomputed target through would
remove that directly and, more importantly, shorten the critical section, which
is where the super-linear amplification lives. This is narrower and better
evidenced than the general "interpreted-dispatch cache" the old doc proposed.

## Also worth fixing, found while diagnosing the above

**`ClassCastException` renders an array receiver by its component name.** The
`TestObjectDataType` failure read `java.lang.String cannot be cast to
java.lang.String` when the receiver was a `String[]`. That message cost real
diagnosis time — it looks exactly like a class-identity split, which is what it
was chased as. The fix belongs wherever the CCE text is built; it is
independent of everything above and is not blocked by any of it.

## Reproducing the A/B

```bash
cd apps/h2database-suite-runner
ONLY='TestStreamStore|TestFreeSpace|TestNestedJoins'
# lifted arm — add CRATONVM_JIT_ALLOW_PACKAGES; omit it for the control arm
TMPDIR=/data/tmp H2_ROOT=/data/data/h2database/h2 CRATONVM_BIN=<binary> \
  CRATONVM_JIT_ALLOW_PACKAGES='org/h2/' \
  OUTROOT=<out> ./run-h2-suite.sh run --category all --only "$ONLY" --tag lifted
```

`TMPDIR=/data/tmp` is required on the Azure host: `/` is full and the runner's
internal `mktemp` silently produces empty results otherwise. That same full
root filesystem also breaks `cargo build` — the `cc` invocations for
`zstd-sys`/`libsqlite3-sys`/`libmimalloc-sys` die with "No space left on
device" writing to `/tmp` — so build with `TMPDIR=/data/tmp/build`.

## Related

- `vm/src/jit/skip_list.rs` — the `HIB-LONGTAIL.1` comment, which carries the
  same correction as this doc.
- `docs/known-issues/jit-ban/jit-ban-sweep-20260725.md` — the sweep this came out of.
- `docs/known-issues/full-ban-inventory-status-20260726.md` — the cross-session
  ban tracker.
- `docs/internal/jit-bans/hib-antlr-1-removed-shadowed-20260726.md` — the
  `org/antlr/v4/runtime/` half of this same ban. **Removed 2026-07-27**: the
  H2 suite never exercises ANTLR, and a 57-class Hibernate HQL A/B came back
  identical. HIB-LONGTAIL.1 is `org/h2/`-only now, so the three residuals
  below are all that is left of it.
