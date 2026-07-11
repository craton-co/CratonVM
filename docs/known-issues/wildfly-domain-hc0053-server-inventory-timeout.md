# WildFly domain boot: `WFLYHC0053` — Host Controller never gets the Process Controller's server inventory within 30 seconds

Status: OPEN
Severity: High (gates a domain boot from ever reaching a fully-up managed-server state under CratonVM; Host Controller itself boots and stays healthy, but no managed application server ever starts)
First confirmed: 2026-07-11, split off from `docs/internal/wildfly-domain-heap-corrupt-value-timeout-RESOLVED.md` (that doc's own investigation chain hit this as its new front-line blocker once its original subject, the `HIB-CV-32` heap-integrity guard, was confirmed resolved — see that doc for the full 2026-07-05 through 2026-07-11 history that led here)

## Relationship to the retired `HIB-CV-32` doc

This bug is **not** the same issue as `HIB-CV-32` (`gen_heap::read_slot: corrupt Value cell`), which that doc tracked and which is now confirmed resolved. `WFLYHC0053` is a distinct, later-stage failure: it only becomes reachable *after* Host Controller boots cleanly past everything HIB-CV-32 and its surrounding fixes (SIGSEGV, MSC `provides()`/`isShutdown()`/demand-propagation, `BufferedOutputStream.flush()`, the `CRATONVM_JAVA_HOME` harness fix) were resolved. Do not conflate the two, and do not treat this doc's closure as a precondition for the other's — they are independent.

## Symptom

With a correctly-configured harness (see "Reproduction recipe" below), a WildFly 32.0.1.Final domain boot proceeds through Host Controller startup, extensions, Elytron, `host=foo:add()`, and subsystem activation, then Host Controller's `start-servers` operation fails:

```text
[Host Controller]     java.lang.RuntimeException: WFLYHC0053: Could not get the server inventory in 30 seconds
[Host Controller]     "failure-description" => "WFLYCTL0158: Operation handler failed: java.lang.RuntimeException: WFLYHC0053: Could not get the server inventory in 30 seconds",
```

followed shortly after by:

```text
[Host Controller] DEBUG [org.jboss.as.host.controller] process controller connection closed.
```

Host Controller itself then completes its own boot and logs:

```text
INFO [org.jboss.as] WFLYSRV0025: WildFly Full 32.0.1.Final (WildFly Core Unknown) (Host Controller) started in 143128ms - Started 0 of 0 services (0 services are lazy, passive or on-demand) - Host Controller configuration files in use: domain.xml, host.xml
```

i.e. Host Controller is healthy, stable, not crashed, not livelocked, not OOM'd — but no managed application server ever starts (`Started 0 of 0 services`). This is the furthest point this whole investigation chain has ever reached.

## Reproduction recipe

- Pristine WildFly 32.0.1.Final binary distribution (download fresh from `github.com/wildfly/wildfly/releases` — a previously-booted/mutated copy accumulates `.bak`/regenerated config files and is not a clean baseline).
- `bin/domain.sh`.
- `CRATONVM_MSC_REAL_START=1` (drives real `Service.start(StartContext)` MSC callbacks; without it, boot hangs indefinitely much earlier).
- `CRATONVM_JAVA_HOME=<real JDK 25 install dir>` — **load-bearing**, separate from `JAVA_HOME`. `JAVA_HOME` must point at a shim directory containing only `bin/java` (satisfying `domain.sh`'s launcher convention and pointing at the CratonVM binary); `CRATONVM_JAVA_HOME` is CratonVM's own escape hatch (`vm/src/config.rs::resolve_java_home()`) for finding real JDK boot classes when `JAVA_HOME` itself points at a CratonVM shim tree, not a real JDK. Every session before the sixth silently omitted this and got confusing, partial, or outright wrong results as a consequence (see the retired doc's sixth-session update).
- `JBOSS_JAVA_SIZING=-Xms64m -Xmx1536m -XX:MaxMetaspaceSize=256m` (the WildFly-default `-Xmx512m` OOMs during a full domain boot on CratonVM; this is a heap-sizing artifact, not a leak).
- **Use a non-default bind address/port** (e.g. `-b=127.0.0.5x -bmanagement=127.0.0.5x -Djboss.management.http.port=x9990 -Djboss.management.native.port=x9999`) if running on the shared Azure probe host — concurrent sessions' real-HotSpot WildFly testsuites bind the stock `127.0.0.1:9990` and produce a false-positive `WFLYSRV0083 Address already in use` otherwise.
- Reproduces **100% reliably** — every run hits `WFLYHC0053`, though the exact log-line position of the underlying connection closure varies run to run (see "Confirmed non-deterministic" below).

## The byte-level mechanism (root-caused, not yet fixed)

Captured with `CRATONVM_DBG_SOCK=1` and, later, a debug-build (`hc0053dbg` profile, see below) with live `gdb` breakpoints. The exact sequence at the moment of failure (reproduces every run, always within ~1 second of `"Executing two-phase"` being logged for the `start-servers(enabled-auto-start=true)` operation — this is NOT the 30-second internal timeout being slow; the connection-loss event happens almost immediately, and the code then waits out the full 30s on a latch that will now never be counted down):

```text
[Host Controller] TRACE [org.jboss.as.host.controller] Executing two-phase
[dbg-sock] write: sid=1 sent=5 bytes      <- HC sends the request header
[dbg-sock] read: sid=2 got=1  (x5, byte-by-byte)   <- PC receives it
[dbg-sock] write: sid=1 sent=1 bytes      <- HC sends end-of-message marker
[dbg-sock] write: sid=2 sent=5 bytes      <- PC writes its response header
[dbg-sock] write: sid=2 sent=47 bytes     <- PC writes its response payload (47 bytes)
[dbg-sock] write: sid=2 sent=1 bytes      <- PC writes its end-of-message marker
[dbg-sock] read: sid=2 want=1 (blocking on recv...)   <- PC's reader goes back to waiting; PC's socket is NOT closed
[Host Controller] [dbg-sock] read: sid=1 got=1        <- HC reads exactly ONE byte of PC's 53-byte response
[Host Controller] DEBUG [org.jboss.as.host.controller] process controller connection closed.
```

- **`byte value 152` (`0x98`) is the WildFly wire protocol's own legitimate `CHUNK_START` marker byte, not corruption.** An early session misread this as a corrupted opcode (PC allegedly wrote opcode `20`, HC allegedly received `152`); this was a misdiagnosis, corrected by decompiling both `ConnectionImpl$MessageOutputStream.write()` (writer: prefixes every chunk with `hdr[0] = -104` i.e. unsigned 152) and `ConnectionImpl$2.run()`'s `lookupswitch` (reader: `152` = "more data follows, read a 4-byte length next"; `153` = "end of message"). 152 is the correct, expected first byte of every chunk this protocol ever sends.
- **`ServerInventoryImpl.connectionFinished()`** (traced via `javap` on `wildfly-host-controller-24.0.1.Final.jar`) is the exact source of the "process controller connection closed." line — a `ProcessMessageHandler` connection-lifecycle callback that does **not** count down `processInventoryLatch` (only the real `handleProcessInventory(Map)` success callback does that, per `ProcessControllerConnectionService$2`). So once this callback fires, `determineRunningProcesses()`'s `processInventoryLatch.await(30, SECONDS)` is guaranteed to time out and `WFLYHC0053` is guaranteed to fire — this exactly explains the observed symptom once the connection closes.
- **The read task's socket-read sequence stops dead after the single valid `152` byte** — confirmed via reliable, per-process, sequence-numbered file traces across 5+ fresh full-boot captures: no further `PRE-READ` for `readInt`'s four bytes ever appears. Per `ConnectionImpl$2.run()`'s decompiled exception table, any exception between reading `152` and reaching `readInt()` (i.e. only `new Pipe(8192)` construction + `readExecutor.execute(task)` submission sits in that window) is caught by a catch-all handler that does **not** log anything, safely closes the pipe, and calls `ConnectionImpl.closed()` (→ `ServerInventoryImpl.connectionFinished()` above) before re-throwing — matching every observed symptom exactly (no error/exception text anywhere before the closure).
- **Two independent debug-build `gdb` captures (10th session) with 5 breakpoints total** (`net_phase_e.rs`'s blocking-read line, `close()`, `shutdownInput()`, `shutdownOutput()`, and the separate `phases_early.rs` `close()`) show **exactly one successful read (`Ok(1)`, the `152` byte) and zero further hits on any of the other four** across two full boots reaching the closure. This conclusively proves the Rust-native socket layer is not touched again after that single read — **the mechanism is purely at the Java bytecode level**, most likely inside `ConnectionImpl$2.run()`'s `new Pipe(8192)` construction or `readExecutor.execute(pipeConsumerTask)` submission, or inside `EnhancedQueueExecutor.execute()`'s own real bytecode internals once that submission is accepted.

**Root cause not yet found.** The failure window is narrowed to a few lines of real WildFly/jboss-threads bytecode, but the exact mechanism producing a silent stop (no exception, no native call, no log output) has not been isolated to a specific line.

## Confirmed non-deterministic

Across 5 fresh full-boot runs in one session, `"process controller connection closed."` fired at five different log-line positions (three times around the usual ~17,470-17,477 range on the *second* `start-servers` call, once on the *first*, simpler `start-servers(enabled-auto-start=false)` call at ~34,915, once at ~18,206). This rules out a bug tied to specific message content or a fixed sequence position — it is a genuine race, most likely dependent on host scheduling jitter (the shared Azure probe host typically runs at load average ~11/16 cores with 20+ concurrent user sessions).

## Six hypotheses investigated and refuted (do not re-tread these)

1. **A generic, unconditional `java/util/concurrent/Executor.execute()` native intercepting real executors.** `native-builtins/src/phases_late.rs` registers `"execute", "(Ljava/lang/Runnable;)V"` on the literal interface `java/util/concurrent/Executor` (intended only for `CompletableFuture.delayedExecutor()`'s synthetic object) — a plausible self-deadlock mechanism if it reached real `EnhancedQueueExecutor` instances via interface fallback. **Refuted**: a temporary diagnostic showed zero hits across a full 34,000+-line boot; `EnhancedQueueExecutor.execute()` genuinely dispatches through its own class-exact registration path / real bytecode, never this interface-level native. (Still a latent correctness landmine for some *other* real-`Executor`-implementing class without its own exact-class registration — worth a future narrow cleanup, but not this bug.)
2. **GC-timing correlation.** A tight burst of `gen_heap::mark_young: rejecting object ... with implausible extent 0` / inconsistent-header conservative-rejection warnings was observed near one early capture of the failure. **Refuted**: checked all 5 of a later session's captured closures for the same warnings in the 200 lines immediately preceding each — zero found near any of them. The original correlation was coincidental noise from unrelated allocation activity, not causal.
3. **JIT-related silent truncation** (a previously-documented bug class in this codebase: `UnreachedCode` uncommon-traps on dead `invokedynamic`/`StringConcatFactory` branches silently truncating loop iterations without throwing). **Refuted**: re-ran with `CRATONVM_DISABLE_JIT=1` — the exact same closure reproduces at the same log-line position with JIT completely disabled. The bug is present in pure interpreter execution; not a codegen/uncommon-trap issue.
4. **Uncaught-exception propagation being swallowed somewhere.** Per `ConnectionImpl$2.run()`'s own decompiled bytecode, its catch-all handler re-throws (`athrow`) after cleanup, so the natural expectation was a crashed-thread event. **Refuted (as a *visible* propagation, exact mechanism still open)**: `CRATONVM_DBG_UNCAUGHT=1` (prints any exception that propagates all the way out of a `Thread.run()`) shows zero output across a full boot reaching the closure. Two explanations remain open, neither confirmed: (a) the actual failure is inside `readExecutor`'s own worker-thread infrastructure, which — like real `ThreadPoolExecutor`/`EnhancedQueueExecutor` — is expected to catch `Throwable` internally around a submitted task and not propagate it as a crashed-thread event; or (b) execution never reaches an actual `athrow` at all.
5. **A permanently-blocked Java thread reading the Host Controller process's own real stdin (fd 0).** A live `gdb` capture caught a "main-vm" thread genuinely parked in `libc::read(fd=0, ...)`, traced to `ManagedProcess.start()` writing a `pcAuthKey` to the child's stdin then closing its own end. **Ruled out as the WFLYHC0053 gate** (though it is a real, separate, still-not-fully-identified thread/behavior worth flagging): in three separate full runs, boot continued for tens of thousands of further log lines and reliably reached both `WFLYSRV0025 ... started` and the `WFLYHC0053` failure while this one thread stayed permanently parked on fd 0 — proving CratonVM threads run genuinely independently and this thread is not on the critical boot path. Not further identified which WildFly thread/code this is; the temporary diagnostic used to find it (`CRATONVM_DBG_STDIN_READ=1` guard in `native_fis_read`, `native-io/src/lib.rs`) was reverted, not merged.
6. **`EnhancedQueueExecutor`/`EQE_PENDING` deferred-task-starvation.** `native_exec_execute` (in `native-builtins/src/wildfly_core.rs`) can intercept `EnhancedQueueExecutor.execute(Runnable)` and park the submitted task in a process-global `EQE_PENDING` map, only drained by `AsyncFutureTask.await()` (added for a real, separate, previously-diagnosed WildFly bootstrap-ordering bug, "Round 89") — a very plausible mechanism for silently stranding `readExecutor`'s next-chunk-read task forever with no exception. **Refuted, empirically, before any code change**: (a) `native_exec_execute`'s registration (the enqueue side) is entirely gated behind `CRATONVM_SYNTHETIC_EQE`, which this doc's harness has never set — real jboss-threads bytecode is the default per `wildfly_core.rs`'s own comment ("the synthetic Rust-backed executor below ... is BROKEN as a whole ... Default to the REAL jboss-threads bytecode"); (b) live confirmation via `CRATONVM_DBG_EQE=1` against a fresh full boot reaching the `WFLYHC0053` closure at line 17,509 shows **zero `"[eqe] enqueue pool=..."` lines** in the entire 17,509-line log — the enqueue side never fires. `readExecutor.execute()` runs the real, unintercepted `EnhancedQueueExecutor.execute()` bytecode in this doc's exact configuration.

**Net state after all six:** the failure is known with high confidence to be (a) not a native-socket-layer event (no further read/close/shutdown after the single successful read), (b) not JIT-related, (c) not a genuinely-uncaught exception, (d) not the generic-`Executor`-interface dispatch bug, (e) not GC-timing related, and (f) not the `EQE_PENDING` deferred-queue mechanism. What remains is that `readExecutor.execute()` (real bytecode) and/or the `new Pipe(8192)` construction inside `ConnectionImpl$2.run()`, between reading the valid `152` chunk-start byte and the next expected socket operation, silently fails to let execution continue — via some mechanism that produces no exception, no native call, and no log output.

## Tooling: the `hc0053dbg` Cargo profile

`[profile.release]` uses `debug = "line-tables-only"` plus fat LTO, which optimizes out locals needed for live `gdb` narrowing (confirmed: `gdb` breakpoints inside `net_phase_e.rs::re1_socket_read_stream` reported `No symbol "stream_id" in current context` on ~1,000 consecutive hits). A dedicated, committed, opt-in Cargo profile now exists for this:

```toml
[profile.hc0053dbg]
inherits = "release"
debug = 2
lto = false
codegen-units = 16
opt-level = 1
# opt-level = 0 package-overrides for cratonvm-native-builtins / cratonvm-native-io
```

Build with `cargo build --profile hc0053dbg -p cratonvm-cli` (binary at `target/hc0053dbg/cratonvm`; full rebuild ~7 minutes; runtime is meaningfully slower than release but reaches this doc's `WFLYHC0053` reproduction point in a comparable number of log lines — acceptable for a boot-time, non-throughput-sensitive race). This is what made the two clean, ambiguity-free `gdb` captures above (Findings from the "byte-level mechanism" section) possible, after periodic `gdb` snapshots and continuous `strace` both proved too coarse or too slow.

`strace` notes for whoever continues: `-e trace=network` alone misses the plain `read`/`write` syscalls Rust's `TcpStream` uses on an already-connected socket (only caught 5 `recvfrom`/3 `sendto` across a full run that had dozens of actual reads/writes on the target connection per other traces); `-e trace=network,read,write,close` traces every `read()` process-wide and is too slow to reach the failure point in a reasonable window. A future attempt should filter to a specific fd (`-P /proc/<pid>/fd/<N>` if supported) or attach only in a narrow window using log-line count as a timing signal.

## Recommended next step

**A live, debug-build (`hc0053dbg`) `gdb` session, stepping through `ConnectionImpl$2.run()`'s `Pipe`/`readExecutor.execute()` real-bytecode path line by line**, since every other explanation (native socket layer, JIT, uncaught exceptions, GC timing, generic Executor dispatch, EQE_PENDING) has been ruled out with direct evidence. Concretely, in priority order:

1. Set a conditional breakpoint inside `EnhancedQueueExecutor`'s real bytecode dispatch path specifically. This needs an **interpreter-level** hook (e.g. a temporary debug print in `execute_invoke`/`try_stackless_invoke` gated on method name `"execute"` and descriptor `"(Ljava/lang/Runnable;)V"`, since there is no single Rust function implementing real bytecode's `execute()` to put a native breakpoint on) to directly observe whether `readExecutor.execute(task)` is ever reached, returns normally, or the call never completes.
2. Alternatively, temporarily set `CRATONVM_SYNTHETIC_EQE=1` for a real-JDK-mode boot to force `EnhancedQueueExecutor` through CratonVM's own synthetic, Rust-backed `native_exec_execute` implementation instead of real bytecode — if the bug disappears or changes shape, that isolates the failure to real `EnhancedQueueExecutor` bytecode specifically. **Caution**: `wildfly_core.rs`'s own comments warn this flag has caused *different* regressions before (e.g. `threadStatus` never getting initialized), so treat any change in symptom carefully — it may just trade one bug for another rather than confirm the hypothesis.
3. If both of the above come up empty, build a purpose-built synthetic repro outside WildFly entirely: two real OS processes, a from-scratch `Connection`-like class mirroring `ConnectionImpl`'s exact chunk-framing + `Pipe` + `readExecutor.execute()` dispatch shape, hammered under load — matching this whole investigation chain's own repeated pattern of narrowing a WildFly-specific repro down to a minimal, iteration-friendly synthetic one (e.g. the `FieldSpawn.java` repro used earlier in the retired doc's history for an unrelated AB-BA lock-order bug). A minimal repro would make it far easier to add heavier instrumentation without a rebuild + multi-minute-boot cycle per iteration.
4. Given the confirmed non-determinism, budget for **several** repeated attempts per session — a single capture is not representative; five full ~3-minute boots were needed in one prior session just to observe the closure point shift around.

## Related / do not re-tread

- `native-builtins/src/phases_late.rs::register_phase57_process`'s dead, `wait_with_output()`-based `ProcessBuilder.start()` registration (confirmed dead code, overridden by the live implementation in `native-io/src/process.rs`) is safe, low-risk cleanup debt worth deleting in its own small PR — unrelated to `WFLYHC0053`, flagged only so it isn't mistaken for the live implementation by a future reader.
- The generic `Executor.execute()` interface-name registration in `phases_late.rs` (hypothesis 1 above) is a latent correctness landmine for any real `Executor`-implementing class without its own exact-class registration, even though it's not this bug — worth scoping it to only match the actual synthetic delayed-executor object (e.g. by checking the object's real class name equals a synthetic marker) in a future cleanup pass.
- See `docs/internal/wildfly-domain-heap-corrupt-value-timeout-RESOLVED.md` for the full prior history (2026-07-05 through 2026-07-11) that led to this doc's split-off, including every fix that unblocked boot far enough to reach `WFLYHC0053` in the first place.
