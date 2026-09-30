# `TestWebSocketFrameClient` crashes (thread explosion + watchdog abort) after merging `dev`'s "interpreter round i1" (waves 13-14)

| | |
|---|---|
| **Status** | Open — newly discovered, pre-existing on `dev`'s tip, not caused by this branch's own changes |
| **HotSpot** | Not yet re-checked against this exact commit (was passing on all prior binaries this session) |
| **CratonVM** | Crashes with exit code `-1073740791` (`0xC0000409`, `STATUS_STACK_BUFFER_OVERRUN`) after the VM's own thread-watchdog dumps 624 threads and aborts the process |
| **Discovered** | 2026-09-26, verifying `cratonvm-tomcathsid-fix16-9e444f.exe` (this branch's mirror_pin/LogManager fixes, merged with `origin/dev` at `7eaeecee2`/`f281a3091`, "interpreter round i1 wave 13/14") before pushing |

## How this was found

`TestWebSocketFrameClient` passed cleanly (`OK (4 tests)`) on `cratonvm-tomcathsid-fix15-9e444f.exe` — built from this branch's tip BEFORE merging `origin/dev`. Immediately after merging `origin/dev` (no source conflicts; the merge touched `classloading/`, `jit/`, `vm/src/jvmti/*`, `vm/src/runtime/interpreter/*`, `vm/src/vm/vm_exec.rs`, `vm/src/native/jni.rs` — a large interpreter/JIT/JVMTI change set, nothing in this branch's own diff) and rebuilding (`cratonvm-tomcathsid-fix16-9e444f.exe`), the SAME class hangs then crashes, reproducibly (2/2). Since this branch's own commits are identical between the two binaries, the regression is in the merged `dev` content, not in this branch's mirror_pin/LogManager work — confirmed separately unaffected: `TestDefaultInstanceManager` still passes 3/3 and the rest of the WebSocket cluster's first class (`TestWsWebSocketContainer`) still passes on `fix16`.

## Symptom

```
JUnit version 4.13.2
..Sent Hello message, waiting for data
Received Hello, now sending data
```
...then the second test (`testConnectToServerEndpoint`) never progresses. A `--stack-dump-on-timeout 20` capture shows thread `tid=54` ("http-nio-127.0.0.1-auto-2-exec-2") repeatedly dumped at the same point across successive periodic dumps, inside:

```
org/apache/tomcat/websocket/WsRemoteEndpointImplBase$StateMachine.textStart / .complete
  <- TesterFirehoseServer$Writer.doRun
  <- TesterFirehoseServer$Endpoint.onMessage
  <- PojoMessageHandlerWholeBase.onMessage
  <- WsFrameBase.sendMessageText / WsFrameServer.sendMessageText
  <- WsFrameBase.processDataText / processData / processInputBuffer
  <- WsFrameServer.onDataAvailable / doOnDataAvailable / notifyDataAvailable
  <- WsHttpUpgradeHandler.upgradeDispatch
  <- UpgradeProcessorInternal.dispatch
  <- AbstractProcessorLight.process / AbstractProtocol$ConnectionHandler.process
  <- NioEndpoint$SocketProcessor.doRun
  <- ... ThreadPoolExecutor machinery
```

Note the frame at `WsRemoteEndpointImplBase$StateMachine` moves between `.textStart` (pc=0) and `.complete` (pc=0) across dumps — consistent with the state machine's send-message state transition looping or being repeatedly re-entered rather than making forward progress. The process's own watchdog eventually reports `624 thread(s) dumped; aborting process` and the process exits with `STATUS_STACK_BUFFER_OVERRUN` — almost certainly this VM's own fail-fast abort path (not a real stack-smash), consistent with a very large, unbounded number of threads having been created (each dump/thread-creation cycle presumably spinning up more socket-processor threads as the stuck connection keeps getting redispatched).

## What this is NOT

- Not the `LogManager` concurrent-first-use race this branch's own fix ([38](../../internal/fixed-suite-bugs/tomcat/38-websocket-cluster-logmanager-concurrent-first-use-race-FIXED-20260926.md)) closed — that hang was in `LogManager.demandSystemLogger`, a completely different call stack, and reproduced on binaries from before this branch's fix even existed. This is a new, different failure mode, in WebSocket message-send state machine logic, and only appeared after the `dev` merge.
- Not caused by the mirror_pin ordering fix ([39](../../internal/fixed-suite-bugs/tomcat/39-mirror-pin-registration-ordering-classloader-unload-FIXED-20260926.md)) — that only affects `ClassLoader.defineClass`/mirror-rooting/class-unload timing, unrelated to WebSocket frame state machines, and `TestDefaultInstanceManager` (the fix's own target) is unaffected on the post-merge binary.

## Bisection so far (all on `cratonvm-tomcathsid-fix16-9e444f.exe`, no rebuild required)

- **`--nojit` still reproduces it** (confirmed via `--stack-dump-on-timeout`-free run: thread count climbing past 90+ before the run was killed at the 90s mark, same stall point). This rules OUT the JIT-specific codegen changes (`jit/src/lib.rs`, `jit/src/tiered.rs`, `jit/src/x64/*`) as the sole cause — the bug reproduces in pure interpreter mode too.
- **`WsRemoteEndpointImplBase$StateMachine` is plain instance `synchronized` methods on a `State` enum field** (not `AtomicReference`/CAS) — `public synchronized void textStart() { checkState(...); state = ...; }`. Two standalone probes (`tools/probes/InstanceSynchronizedContentionProbe.java`: 16 threads x 20,000 increments on one shared object's synchronized methods; `tools/probes/ReflectiveInvokeSynchronizedContentionProbe.java`: the same, but through `java.lang.reflect.Method.invoke` — the exact mechanism `PojoMessageHandlerWholeBase.onMessage` uses to dispatch to the `@OnMessage` POJO method, per the stack trace) **both pass cleanly on `fix16`**, ruling out plain instance-synchronized monitor contention AND the reflective-`Method.invoke`-into-synchronized combination as standalone causes.
- This means the bug needs something the probes don't capture: most likely the actual NIO/socket I/O interleaving (a real `SocketProcessor`/poller/worker thread handoff, not a synthetic thread pool), or WebSocket's own byte-buffer/text-decoding state carried across the synchronized boundary. The repeated `tid=54` dump alternating between `.textStart` and `.complete` (both `pc=0`, i.e., at method entry) across successive periodic dumps is consistent with the SAME connection's send being retried repeatedly rather than one thread being permanently blocked acquiring a monitor — worth checking whether a native socket-write return value or exception classification changed under the interpreter round i1 merge in a way that turns a transient condition into an infinite retry (which would also explain the unbounded thread growth, if each retry spins up fresh processor threads).

## Next step

Needs bisection within the merged `dev` range (`7eaeecee2`, `f281a3091` — "interpreter round i1 wave 13/14"). JIT codegen is ruled out (see above); the remaining suspects are the JVMTI changes (`vm/src/jvmti/native_env.rs`, `agent.rs`), the shared interpreter dispatch changes in `vm/src/vm/vm_exec.rs` / `vm/src/runtime/interpreter/invoke.rs` (the by-name/bootstrap-method/native-context-invoke family the merge's own docs describe), or something in `vm/src/runtime/interpreter/gc_and_alloc.rs`'s GC-pause changes interacting with the NIO endpoint's thread pool. A real git-bisect (building at intermediate commits within the merged range) is the reliable next step, but each build in this environment costs on the order of tens of minutes to a few hours depending on machine contention — worth doing only as a dedicated investigation, not embedded in an unrelated fix's verification pass.

## Reproduce

```powershell
cd apps\tomcat-suite-runner
.\run-one.ps1 -Vm craton -Exe <binary built from dev at-or-after 7eaeecee2> -Class org.apache.tomcat.websocket.TestWebSocketFrameClient -TimeoutSec 90
# add --stack-dump-on-timeout 20 to the JVM args for a live trace while it stalls
```

## Interpreter round i1 check (2026-09-26, Linux)

Checked by the interpreter round i1 orchestrator on the Linux build host
(`/data/cvm/apps/tomcat` fixture, the `run-tomcat-suite.sh` arguments, one
class, 150 s cap) against binaries built at each round-i1 wave:

| Binary | `--compatible` | `--jdk-only` (launcher default) |
|---|---|---|
| pre-round baseline (`base`, before wave 9) | OK (4 tests), 50 s | hangs before the first message |
| wave 12 / 13 / 14 / 15 / 16 | OK (4 tests), 48–53 s each (wave 14 and 16 twice) | hangs (waves 12–14 run; same as baseline) |
| HotSpot 25 | OK (4 tests), 0.8 s | — |

So on Linux the rounds do not reproduce the Windows thread-explosion crash
under `--compatible`, and the `--jdk-only` hang predates round i1 (the
baseline binary shows it). The Windows reproduction (`fix16`) was not
bisected; the next step there is the same class on Windows binaries built at
`a4e919977` (wave 12), `f281a3091` (wave 13) and `7eaeecee2` (wave 14), with
and without `--nojit`, to tell a wave from a Windows-only interaction.
