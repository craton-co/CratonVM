# 31 — `synchronized` / lock-bearing code is never JIT-compiled  (OPEN)

**Status:** OPEN. Split out of known-issue 04 (embedded-server throughput
wall) on 2026-07-27, when 04's residual was re-derived end to end; 04 itself is
retired to `docs/internal/` because every defect *it* documented is fixed.

**Why it matters:** this is the measured root cause of the Tomcat
webapp-deploy wall — the largest remaining item in the Tomcat suite. One
`TestHostConfigAutomaticDeploymentAddition` test method takes **245 s on
CratonVM vs 1.92 s on HotSpot (128×)**, and the chain from that number down to
this defect is traced below.

**Affected:** every `synchronized` method and every `synchronized (x) { … }` /
`lock(); try { … } finally { unlock(); }` body in every workload — including
`java.io.ByteArrayInputStream.read()`, `java.io.BufferedInputStream.read()`,
`java.util.concurrent.locks.ReentrantLock`, `StringBuffer`, `Vector`,
`Hashtable`, `PrintStream`, `Random`, and Tomcat's own session/`StringCache`
code.

## 1. The microbenchmark

`apps/tomcat-suite-runner/probes/SyncMethodProbe.java`. The three bodies are
byte-identical apart from the modifier / block; the monitor is always
uncontended (single thread), so this is not lock contention.

| body | CratonVM | HotSpot |
|---|---|---|
| `plain` | 45–120 ns/op | 17–18 ns/op |
| `sync` (ACC_SYNCHRONIZED) | 4.8–16.9 µs/op | 16–23 ns/op |
| `syncBlock` (`monitorenter`/`monitorexit` in the body) | 5.8–18.4 µs/op | 15–24 ns/op |

**50–150× the same code without the monitor**, against ~1× on HotSpot.

`apps/tomcat-suite-runner/probes/MonitorCostProbe.java` narrows that to the
monitor itself: the bodies are as close to empty as Java allows (`return
field;`), so the difference between them is the monitor pair plus two
bytecodes.

| body | CratonVM | HotSpot |
|---|---|---|
| `plain` | 389–793 ns | 0–11 ns |
| `syncThis` / `syncMethod` | 663–1981 ns (**monitor pair ≈ 0.5–1.2 µs**) | 6–29 ns |
| `lockUnlock` — `ReentrantLock.lock()/unlock()` | **15 400–36 600 ns** | 24–39 ns |

So an uncontended `ReentrantLock` round trip costs **~20× an uncontended
monitor** here, where HotSpot prices them the same. Identical with and without
`CRATONVM_REAL_AQS=1`, so it is not the real-vs-synthetic AQS switch. This
matters directly: JDK 25's `BufferedInputStream.read()` takes an `InternalLock`
— a `ReentrantLock` — when virtual threads are supported, which is always on
21+.

## 2. Root cause — two independent refusals, one per form

1. **ACC_SYNCHRONIZED is an unconditional JIT skip.**
   `vm/src/runtime/interpreter.rs` lists `is_synchronized` directly in the
   skip decision (next to `env_disable_jit` / `static_skip_reason` /
   `fjp_skip` / `native_skip`) and then records the method as permanently
   skipped. `CRATONVM_DBG_JITC` shows *no* `bg-compile` line for such a method
   at all — an absence, not a bail. The same gate is repeated in
   `try_jit_compile_callee_slow` and in both direct-callee resolvers.

2. **A `synchronized` BLOCK is refused by RBC.6.** javac compiles it as a
   protected range whose catch-all handler does `aload <monitor-local>;
   monitorexit; athrow`. That handler reads a local that is not an incoming
   parameter, so `local_handler_reads_unsafe_local` is true, and the escape
   hatch `precise_handler_frames_enabled` is **default-OFF** pending a
   separate live correctness bug —
   `../jit-precise-handler-frame-drops-live-locals-20260727.md`:

   ```
   [cratonvm-jitc] bg-compile SyncMethodProbe.syncBlock(II)I tier=C1 optimized=false
   [cratonvm-jitc] resolver-bail site=rbc6-handler-reads-unsafe-local SyncMethodProbe.syncBlock(II)I
   [cratonvm-jitc] compile-bail SyncMethodProbe.syncBlock(II)I backend_attempted=false
   ```

   Three bails and `MAX_TIER_FAIL_RETRIES` make it permanent. **Note:** the
   escape hatch is *not* what is blocking the I/O path in §3 — turning
   `CRATONVM_JIT_PRECISE_HANDLER_FRAMES=1` on changes those numbers by 0 %
   (measured). Form 1 is the dominant one there.

## 3. How this produces the Tomcat deploy wall

`--stack-dump-on-timeout=90` on a single deploy test method
(`RunMethods … testAdditionWarAddDir`) puts the main thread here:

```
 46 ContextConfig.webConfig
 47 ContextConfig.processClasses
 48 ContextConfig.processAnnotations
 49 ContextConfig.scanWebXmlFragment
 50 ContextConfig.processAnnotationsUrl
 51 ContextConfig.processAnnotationsJar
 52 org/apache/tomcat/util/bcel/classfile/ConstantPool.<init>
 53 org/apache/tomcat/util/bcel/classfile/Constant.readConstant
 54 java/io/BufferedInputStream.read          <-- one call PER BYTE
 55 java/io/BufferedInputStream.read1
```

Tomcat's annotation scan parses **every class in every scanned JAR** with its
own BCEL reader, which pulls bytes one at a time through a `DataInputStream`
over a `BufferedInputStream`. `apps/tomcat-suite-runner/probes/ByteReadProbe.java`
prices that operation directly (256 KiB, ns per byte):

| layer | HotSpot | CratonVM | ratio |
|---|---|---|---|
| `bulk` — `readAllBytes` + array index | 2.8–5.2 | 118–253 | ~50× |
| `arrayStream` — `ByteArrayInputStream.read()` | 0.5–38 | 1016–1161 | ~100× |
| `bufferedOverArray` — `BufferedInputStream.read()` over memory | 19–30 | 4712–5000 | **~200×** |
| `buffered` — same over a `FileInputStream` | 21–33 | 2538–5055 | ~180× |
| `data` — `DataInputStream.readUnsignedByte()` | 21–39 | 13069–15937 | **~500×** |

Two things to read out of that table:

* `buffered` ≈ `bufferedOverArray` ⇒ **the buffering itself works**; the file
  descriptor behind it is not the problem. Each *wrapper layer* is.
* Every layer in the chain is a `synchronized` method
  (`ByteArrayInputStream.read()`) or a `lock(); try { … } finally { unlock(); }`
  body (`BufferedInputStream.read()` in JDK 25, plus all of `ReentrantLock`/AQS
  underneath). By §2, none of them is ever compiled, so each byte pays
  microseconds of interpreter.

At ~15 µs per byte, a few hundred class files of a few kilobytes each is
minutes — which is exactly the 245 s.

## 4. What this is NOT

* **Not a JIT hot-path problem.** A deploy runs at the same speed with the JIT
  off: `TestApplicationFilterConfig` = 11.1 s (JIT on) vs 10.8 s (JIT off).
  Every hot-loop lever — root-snapshot caching, direct calls, the C2
  exception-table exclusion — is irrelevant to it.
* **Not VM start-up cost.** Bare start-up on the same classpath is 0.36 s
  (CratonVM) vs 0.08 s (HotSpot); 0.28 s of the 245 s.
* **Not lock contention** — the probes are single-threaded and uncontended.
* **Not the monitor helpers.** The backend *does* lower `monitorenter` /
  `monitorexit` (`Compiler::emitted_monitor_call`), and
  `precise_exception_frame_sites_supported` already whitelists both opcodes.
  Form 2 fails before the backend is reached.
* **Not `StringBuffer` specifically.** `StringBuffer` measures the same as
  `StringBuilder` here (1.0×/0.9×/0.7×) because both are served by Rust
  shadows that bypass the Java monitor. The cost lands wherever there is no
  shadow — application code and the JDK I/O stack above.

## 5. Fix sketches (ranked)

1. **Form 1 — hold the monitor around the compiled body, not inside it.** The
   compiled code need not know about the monitor: the JIT invocation path can
   acquire the receiver/class monitor before entering and release it on every
   exit, reusing the existing GC-safe monitor acquire (which returns the
   possibly-relocated ref) instead of new prologue/epilogue codegen.
   **Settle first:** a deopt or OSR transition hands the frame to the
   interpreter, which has its own `Frame::monitor_on_exit` release — exactly
   one of the two must run. And every entry that can reach a compiled body
   must be covered: `execute_jit_call`, the dispatch helpers, the compile-time
   `direct_calls` plan, and `jit_invoke_virtual_mic`'s cached `entry_ptr`.
   Today all four refuse `is_synchronized`, so the audit is "keep all four
   refusing, or teach all four to wrap" — a missed one is a deadlock, not a
   wrong answer, so it needs its own soak.
2. **Form 2 — admit the javac monitor-handler shape specifically.** That
   handler reads exactly one non-parameter local, the monitor object, which
   `monitorenter` stored from a value the compiled code still holds. That is a
   far narrower promise than the general precise-frame handoff currently
   default-OFF, and could be admitted on its own pattern.
3. **Cheap partial:** intrinsify the three JDK I/O entry points the annotation
   scan actually uses (`ByteArrayInputStream.read()`,
   `BufferedInputStream.read()`, `DataInputStream.readUnsignedByte()`), guarded
   on the receiver's class being exactly that class so a subclass override is
   never shadowed. This buys the deploy wall without touching the JIT, but it
   is a shadow of real, present Java code and leaves every other
   `synchronized` body slow — prefer 1 and 2.

## 6. Reproduction

```
# the two forms, in isolation
cratonvm.exe -Xmx2g -cp <probes-out> SyncMethodProbe 100000
CRATONVM_DBG_JITC=1 cratonvm.exe -Xmx2g -cp <probes-out> SyncMethodProbe 3000

# the monitor / ReentrantLock pair on its own
cratonvm.exe -Xmx2g -cp <probes-out> MonitorCostProbe 200000

# the per-byte I/O price the deploy pays
cratonvm.exe -Xmx2g -cp <probes-out> ByteReadProbe

# one real deploy, and where it sits (needs the tomcat suite classpath)
apps\tomcat-suite-runner\run-one.ps1 -Exe <exe> -Main RunMethods -ExtraCp <probes-out> `
  -Args2 org.apache.catalina.startup.TestHostConfigAutomaticDeploymentAddition,testAdditionWarAddDir
cratonvm.exe -Xmx2g --stack-dump-on-timeout=90 -cp <cp> RunMethods <class> <method>
```

Probe sources: `apps/tomcat-suite-runner/probes/{SyncMethodProbe,ByteReadProbe,RunMethods}.java`.
