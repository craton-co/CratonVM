# WildFly standalone boot: residual "different extension missing each run" + STW JIT-takeover stall

Status: OPEN — split off 2026-07-11 from
[[wildfly-standalone-managed-server-boot-fails-under-surefire-fork]] (FIXED at
`docs/internal/fixed-suite-bugs/wildfly-standalone-managed-server-boot-fails-under-surefire-fork.md`).
Updated 2026-07-11 (follow-up session): two more concrete stale-`ObjectRef` sites found and fixed
(`native-io/src/stream_decoder.rs`, `native-io/src/stream_encoder.rs`), raising the fixed rate from
6/10 to 9/10 in the same sample. The residual is now narrower and better characterized — see below.
Severity: Moderate — down from "blocks most classes" to "blocks a minority, non-deterministically" for
`testsuite/integration/basic`.

## Background

The linked fixed-suite-bugs doc root-caused a stale-`ObjectRef`-across-GC bug in
`create_constructor_object`/`create_method_object`/`create_field_object`
(`native-builtins/src/lang_class.rs`) that corrupted several WildFly `Extension` SPI instances loaded
via `ServiceLoader` during standalone-server boot, cascading into a
`NullPointerException: ... "this.controller" is null` crash in `AbstractControllerService` — exit code
1, before `server.log` was ever written. A first verification pass (10-class sample) showed only 6/10
classes clearing the crash; this doc originally tracked that 40% residual.

## Follow-up (2026-07-11): two more sites of the identical pattern found and fixed

`native-io/src/stream_decoder.rs::alloc_stream_decoder` and
`native-io/src/stream_encoder.rs::alloc_stream_encoder` had the exact same defect: each holds its
`is`/`os` (`InputStream`/`OutputStream`) parameter — extracted by the caller from its own native args
before either function is called — across `ctx.ensure_class_initialized("sun/nio/cs/StreamDecoder"
/StreamEncoder")` (runs `<clinit>`, allocating) and `ctx.alloc_object(...)`, then stores the
now-possibly-stale reference into the new `StreamDecoder`/`StreamEncoder`'s `in`/`out` field. Every
`InputStreamReader`/`OutputStreamWriter` construction anywhere in the JVM (including WildFly's own
`FileReader`-based config/log parsing, and JDK-internal usages during class loading) goes through this
path, so under WildFly's boot-time classloading pressure this was at least as impactful as the
originally-fixed `lang_class.rs` sites. Fixed with the same pin/re-read pattern
(`ctx.pin_native_root`/`ctx.read_native_pin`/`ctx.unpin_native_roots`).

**Verified impact:** re-running the identical 10-class sample against a binary with all three fixes
(`frozen-cratonvm-wf-surefire-boot-20260711-v3.bin`) raised the "clears the original crash" rate from
6/10 to **9/10**. Two classes that were previously deterministically/mostly hitting the original
`this.controller is null` NPE (`SharedBeanInEarsUnitTestCase`, `MetadataCompleteCustomDescriptorTestCase`)
now clear it every time observed.

## What's left: a genuinely non-deterministic, likely-concurrent extension-loading race

The one remaining class in the 10-class sample
(`org.jboss.as.test.integration.ejb.singleton.reentrant.SingletonReentrantTestCase`) does **not**
deterministically hit the original crash — 3 repeated runs against the *same* v3 binary gave 3
*different* outcomes:

- Run 1 (13s): `this.controller is null` (the original NPE).
- Run 2 (71s): a different, later-stage failure (matches the "boots far further" cluster).
- Run 3 (8s): **`IllegalStateException: WFLYCTL0153: No META-INF/services/org.jboss.as.controller.Extension
  found for org.jboss.as.ee`** — via `DeferredExtensionContext.load()` → `FutureTask.get()` →
  `ExecutionException`.

A separate earlier run of a *different* class hit the same `WFLYCTL0153` message for a *different*
extension: `org.jboss.as.jpa`. Different extension missing each time, same class, same binary, no
`ServiceLoader: Constructor.newInstance failed with an internal VM error` warnings present (ruling out
a recurrence of the original fixed bug). This is strong evidence the mechanism is **not** a single fixed
unprotected-`ObjectRef` site (which would reproduce identically every time, like the original bug did
100% of the time pre-fix) — it's timing/scheduling-dependent, and `DeferredExtensionContext.load()`'s
own name (and its use of `FutureTask`/`ExecutionException`) suggests WildFly loads at least some
extensions via a **concurrent, `FutureTask`-based path** — i.e. a genuine multi-threaded race in
extension resolution, not (only) a missing `pin_native_root` call in one Rust function. Whoever picks
this up next should start by reading `DeferredExtensionContext`'s actual boot-time call pattern
(decompile from `/data/data/m2-repo-wildfly-bugbash/repository/org/wildfly/core/wildfly-controller/*.jar`)
to see which extensions get deferred to a background thread vs. loaded synchronously, and whether the
native classpath/module-resolution code that backs `ClassLoader.getResources("META-INF/services/...")`
is safe to call concurrently from multiple threads without external synchronization.

## Also confirmed still present: STW cross-thread JIT-takeover stall (pre-existing, already tracked)

Several of the "boots much further, still doesn't pass" classes (`FlushOperationsTestCase`,
`DataSourceDefinitionTestCase`, `DisabledValidationTestCase`, etc., each running 60-82s before
Arquillian's own ~60s client-side timeout gives up with `LifecycleException: Could not start
container`) show:

```text
[WARN] cratonvm_vm::runtime::interpreter: STW cross-thread JIT takeover is still waiting for
  cooperative mutators rounds=64 pending=2..5 taken=0
```

immediately preceded by the same `gen_heap::guard` "out-of-bounds field read/write dropped ...
class_id=ClassId(0) class_name=java/lang/Object" corruption fingerprint. This is the **same symptom
shape** (`rounds=64`, `taken=0`) as the STW cross-thread JIT-takeover deadlock documented in
`docs/internal/fixed-suite-bugs/wildfly-gc-barrier-boot-hang-and-harness-fixes.md` — that doc fixed the
*main-thread-stuck-in-`pthread_join`* instance of this stall (`bee86ff0`), but explicitly flagged an
**EnhancedQueueExecutor-worker-parked-in-futex** instance (the `jboss-threads` executor WildFly's own
subsystems use extensively) as a **distinct, unfixed residual**, with two candidate fixes described but
deliberately not attempted due to "deep GC-barrier work, high regression risk". This investigation found
no evidence that residual has been closed since — the symptom (same `rounds=64 pending=N taken=0` shape)
still reproduces on current dev. **Do not re-attempt a fix here without a live gdb attach at the exact
stall** (the "poll-and-pounce" technique — see the referenced doc) to confirm which thread/primitive is
actually stuck; a `pgrep -f org.jboss.as.standalone` + `gdb -p <pid> -batch -ex "thread apply all bt"`
9-10 seconds after the child process starts (once `CRATONVM_DBG_STW_CENSUS=1` shows a `rounds=64`
warning in the class's `-output.txt`) is enough to catch it, but this investigation's own attempt missed
the window (the process had already been killed/exited by the time gdb attached) — timing needs to be
tuned per-class since the stall's actual onset varies (we observed it starting anywhere from ~8s to
~60s into boot across different classes/runs).

## Suggested next steps

1. **Higher priority — the `DeferredExtensionContext`/`FutureTask` race**: this is the more novel,
   less-previously-investigated finding. Reproduce with a tight retry loop (3+ attempts of the same
   class against `frozen-cratonvm-wf-surefire-boot-20260711-v3.bin`) and capture `CRATONVM_DBG_STALE_RECV=1`
   / `CRATONVM_DIAG_SERVICELOADER=1` output across enough attempts to catch the exact moment an extension
   goes missing, rather than relying on the after-the-fact WFLYCTL0153 message.
2. **Lower priority, already deeply characterized — the STW JIT-takeover stall**: re-attempt the
   live-gdb-attach technique with better timing (poll for the child PID, then poll the `-output.txt` for
   the `rounds=64` warning itself — not a fixed sleep — before attaching), to get a fresh, authoritative
   thread-state capture. Only then consider one of the two candidate fixes documented in the referenced
   `wildfly-gc-barrier-boot-hang-and-harness-fixes.md` doc, given their explicitly-flagged high regression
   risk.
3. Once both are addressed (or better understood), re-run a full-suite round (not just a 10-class
   sample) for an accurate before/after count against the round-6 baseline (583/605, 96%).

## Evidence

```text
Azure host 20.83.144.174, worktree /data/data/wt-wf-surefire-boot-20260711 (repro harness copy)
Frozen binaries:
  frozen-cratonvm-wf-surefire-boot-20260711-v2.bin (lang_class.rs fix only — 6/10 sample)
  frozen-cratonvm-wf-surefire-boot-20260711-v3.bin (+ stream_decoder.rs/stream_encoder.rs fixes — 9/10 sample)
/data/data/wt-wf-surefire-boot-20260711/sample_results_v3.txt — full 10-class verdict log
/data/data/cratonvm/apps/wildfly/testsuite/integration/basic/target/surefire-reports/
  org.jboss.as.test.integration.ejb.singleton.reentrant.SingletonReentrantTestCase-output.txt
  (captured across the 3 non-deterministic retries: original NPE / different-failure / WFLYCTL0153-ee)
gdb_pounce.sh / gdb_pounce.out — the live-attach attempt that missed the stall window (process had
  already exited by the time gdb attached); worth another attempt with tighter timing
```
