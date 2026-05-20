# Continue prompt — CratonVM real-Java-app gauntlet

You are continuing work on **CratonVM**, a JVM written in Rust at `C:\craton\CratonVM`.
(The project was previously called "rust-jvm"; it has been renamed to CratonVM —
the binary is `target/release/java.exe`, with `rustjvm.exe` as an alias.)

Real JDK 25 boot classes: `C:/Program Files/Java/jdk-25`.
Branches: `main` and `temp` are kept in sync; agent work lands on
`worktree-agent-*` branches and is merged in.

## Goal

Every app in `apps/TARGET_APPS.md` (50+ upstream Java apps — WildFly, Tomcat,
Keycloak, Netty, Cassandra, Kafka, …) must **start AND pass its e2e test suite**
on CratonVM, running real `.class` bytecode under the **JIT**, with **no
synthetic stubs** in the VM. Working pool size 10–15; when an app passes it is
deleted from `apps/` and replaced from the master list.

## CRITICAL — top of the queue

### JIT OSR miscompilation: counted loops never terminate

This is the single most important open bug. Two independent benchmark agents
confirmed it.

- Any **OSR-compiled counted loop in a non-entry static method** hangs forever
  once it runs past the OSR back-edge threshold (`osr_threshold: 10000`,
  `jit/src/runtime/jit_integration.rs:55`).
- Exact reproduction: `bench/LoopProbe2.java` — `java -cp bench LoopProbe2 0 5000000`
  hangs with JIT on, completes correctly with `RUSTJVM_DISABLE_JIT=1`.
- A method with n=100 completes; n=100000+ hangs. `fib(42)` works because pure
  recursion never triggers OSR. Inline loops in `main` work. Only OSR-compiled
  methods break.
- Diagnosis: OSR (on-stack replacement) entry mis-transfers either the loop
  induction variable or the exit-condition register, so the compiled loop's
  exit test never becomes true. The recent commit
  `48c2e72 "jit: fix OSR trampoline callee-saved spill order mismatch"` reduced
  but did NOT eliminate this. Earlier the same path produced a SEGV; after
  `a615bad` (putfield→needs_heap) the SEGV became a hang.
- Impact: every long-running counted loop in application code deadlocks under
  the default (JIT-on) configuration. This blocks almost all real-app progress.

Fix the OSR counted-loop register transfer in `jit/src/` (`x64.rs` /
`runtime/jit_integration.rs` / the OSR trampoline). Verify with `LoopProbe2`
under JIT-on, then re-run the gauntlet.

### GC false-root over-collection (fix written, uncommitted)

`gc/src/gen_heap.rs` has an **uncommitted working-tree change** that fixes a
real bug: the conservative-root scanner had a hardcoded
`MAX_SANE_OBJECT_SIZE = 64 MB` cap and would reject a legitimate 64 MB `int[]`
as a "suspected false root", then collect the live array →
`ArrayIndexOutOfBoundsException`. The fix replaces the byte cap with an
arena-bounds check (a real object fits within the arena it was allocated from
by construction). **Build and verify this change, then commit it.** It was
left uncommitted only because this session was instructed not to rebuild.

### GPU build bit-rotted

`cargo build --release --features gpu-driver` (and the `gpu` stub feature) fails
with ~50 compile errors in `rustjvm-vm`: missing `jit_cuda::annotations`,
`cuda_bridge::Stream` / `Event`, `bytemuck`, `GPU_CRITICAL_COUNT`. The
GPU-offload feature has not been kept compiling. `cuda-bridge` itself and
`jit-cuda` DO compile (including `cuda-bridge --features cuda`); the breakage is
the `gpu-offload` wiring inside `rustjvm-vm`. GPU benchmarking is blocked until
this is repaired.

### JUnit ConsoleLauncher does not run on CratonVM

`junit-platform-console-standalone` throws a picocli exception inside the
launcher and triggers GC heap-walk corruption ("implausible object size"
warnings), exiting -1. This blocks running upstream apps' real JUnit/Surefire
suites — task `#10` in the gauntlet. Likely related to the OSR bug and/or the
GC non-moving-sweep heap walk; revisit after the OSR fix.

## Current app status (real bytecode, JIT default)

Genuinely passing e2e: the probe apps `enumtest`, `cipher_probe` (+`Tiny1..7`),
`sig_probe` (real RSA-2048 + ECDSA-P256), `cleaner_probe`, `cglib_probe`,
`slf4j`; **ActiveMQ** `--version`; Hadoop + HBase `VersionInfo` (already
retired from the pool). NOTE: `sig_probe` still emits two non-fatal
`gc::guard` "out-of-bounds field write" warnings during the X500Principal step
— investigate.

In progress (each driven one real failure deeper per loop iteration; many
are now gated behind the JIT OSR hang above):

| App | Furthest reached this session | Next blocker |
|---|---|---|
| WildFly 32 / Keycloak 16 | boot past jboss-modules launcher | module-load grind |
| Keycloak 26 | deep config bootstrap, ~15+ real frames | `IllegalArgumentException: 1 > 0` |
| Netty | NioEventLoopGroup, WEPoll selector opens | `DefaultChannelId` linkage / echo not yet completing |
| Tomcat 10 | boots past glob + regex + FIS, no longer segfaults under JIT | `Catalina.load` reflection |
| Felix | OSGi framework now initializes cleanly | bundle activation |
| Jetty | `processCommandLine` returns real StartArgs | not recently re-driven |
| Cassandra | NodeTool runs, exits silently | no version output yet |
| Solr | classpath too long for one CLI arg | needs an args file / shorter cp |
| Hazelcast | real bytecode into JDK XPath | XPath `NullPointerException("charset")` — charset SPI bootstrap |
| Kafka | prints real `kafka.Kafka` USAGE | needs `server.properties` to boot the broker |

## What was fixed this session (all merged to main + temp)

Foundational VM bugs — these are the high-value fixes:

- **Class-file reader**: nested-attribute body-offset double-count (`ByteView`
  panic that blocked every app); attribute force-decode for lazy `Raw` attrs.
- **Class-init**: missed-notification race in `ensure_class_initialized_shared`
  (30 s stalls); stale Keycloak `Profile` post-clinit fixup removed.
- **Exceptions**: `auto_box_return` for void `MethodHandle.invokeExact` was
  swallowing thrown exceptions → silent `rc=0` exits; JIT dispatch error
  handler turned catchable native exceptions into fatal `InternalError`;
  bytecode-verifier subtype regression (`is_subclass` returned `false` for
  not-yet-loaded classes — exception classes routinely unloaded at verify
  time); real stack-trace capture in `Throwable.<init>`.
- **JIT codegen**: prologue dropped Java params beyond the register file on
  Win-x64 (`needs_heap` consumed `ARG_REGS[0]`); array-receiver virtual
  dispatch resolved to the component class, not the array class; array
  `checkcast`/`instanceof` used class-hierarchy lookup that can't see array
  assignability; `Arrays.fill` skip-list entry for the OSR miscompile (now a
  symptom of the OSR bug above); OSR trampoline callee-saved spill-order
  mismatch; `putfield` did not set `needs_heap` → VM pointer read from saved
  RBP and corrupted the heap.
- **System.arraycopy**: per-element store check compared class-ids, but every
  primitive array carries the synthetic `ClassId(0)` → `int[][]` copies
  wrongly threw `ArrayStoreException`.
- **Collections**: `TreeMap`/`TreeSet` real-JDK layout independence (side-table
  overlay); `Object.clone` for arrays and `LinkedHashMap`; unmodifiable views
  (real `UnsupportedOperationException` on mutators); `LinkedList` positional
  `add(int,Object)`/`remove(int)` and `toArray(T[])` were unregistered →
  silent data loss; `Collections.newSetFromMap` built a malformed `HashSet`;
  `ConcurrentHashMap.keySet`/`values` covariant-return dispatch.
- **I/O / classloading**: `FileInputStream`/`FileOutputStream` stored the raw
  fd as `Int` in the reference-typed `fd` slot (clobbered to null →
  `close()` NPE, silent empty writes); `URLClassLoader` resolution; jar-`FileSystem`
  mounting via `newFileSystem(Path,Map)`; `resource:` URL scheme;
  `ClassLoader.findClass` native no longer shadows Java subclass overrides;
  `URI.toURL()` now throws `MalformedURLException` for non-URL schemes; MSYS
  POSIX-path normalisation in `parse_classpath`; `Matcher.find()` zero-width
  match infinite loop; `Class.field_at_index` skipped interleaved statics.
- **Natives**: JCA `KeyPairGenerator`/`KeyFactory`/`Signature` synthetic-field
  slots collided with the real JDK 25 layout; log4j `LogManager` /
  `core.Logger` setters; JUL `logp`/`isLoggable`; `PrintStream.write(String,
  int,int)`; `Thread$FieldHolder` now populated so real `Cleaner.create()`
  runs (4 synthetic Cleaner/Thread shims removed); JEP 498
  `sun.misc.unsafe.memory.access=allow`; Windows NIO `WEPoll` selector
  natives over `WSAPoll`; headless `GraphicsEnvironment`/`Toolkit`/`Font`
  natives.
- **GC**: non-moving young-gen mark-sweep fallback when JIT frames are on the
  stack (the moving collector used to skip entirely → young-gen OOM).
- **Shims removed** (per "no synthetic stubs"): demo/Spring CCPP no-ops,
  wildfly_method_synth, wildfly_extras, keycloak16_extras, jboss synth-main,
  felix/jetty/activemq/hadoop/hbase/hazelcast/grpc/ignite/flink/spark
  `*_extras` main short-circuits, the global picocli `<clinit>` no-op.
- **jit-cuda correctness**: `i2c` was lowered as a *signed* truncation (must
  zero-extend — `char` is unsigned); counted-loop recogniser now validates the
  exit comparison + `iinc` stride + start value and rejects non-canonical
  loops instead of mis-lowering; array opcode↔element-kind mismatch is now
  rejected; analyzer runs the specific opcode rejections before the
  `ReductionNotImplemented` shape check.
- **cuda-bridge**: `--features cuda` build fixed + a CI workflow added;
  kernel-arg lifetime invariant made explicit; perf (slice-copy instead of
  `to_vec`, kernel-name interning instead of unbounded `Box::leak`,
  scratch-pool for launch args, overflow guard in `zeros`).

## Known stubs / audits not yet acted on

- **jit-cuda reductions** (`sum`/`dot`/`min`/`max`): no GPU reduction lowering
  exists — array-in/scalar-out is rejected. The audit recommends a 4-agent
  split (emitter shared-mem/atomics, analyzer admission, block-reduction
  codegen, launch/marshalling). `frem`/`drem`/`lcmp`/float-compares and
  `dup2_x1`/`dup2_x2` are also un-lowered (small, low priority).
- **cuda-bridge stubs** (perf-only, no incorrect results): the "pinned memory"
  upload path is a no-op wrapper; the 3-stream "async pipeline" is fully
  synchronous; both gated on the `--features cuda` build.

## How to run the gauntlet

1. Build: `cargo build --release` (from `C:\craton\CratonVM`).
2. Run each pool app on real bytecode with `target/release/java.exe
   --java-home "C:/Program Files/Java/jdk-25" …`; capture the first real
   failure.
3. Dispatch parallel opus fix-agents — **each in its own isolated worktree**
   (the `Agent` tool's `isolation: "worktree"`). Agents that edit the shared
   live worktree get their work clobbered by concurrent commits — this
   happened and cost real work; always isolate.
4. Each agent: one bug, self-contained prompt with the exact repro, a build
   instruction, and "no synthetic stubs / fix the real VM bug".
5. Collect the `worktree-agent-*` branches, merge into `main`, resolve
   conflicts inline (preserve both where independent, pick the better when
   they overlap), rebuild, retest, commit, push.
6. Merge `main` → `temp`. Keep both branches in sync.

## Notes / pitfalls

- Agents sometimes mis-report which branch they committed to, or collide on a
  branch name — always verify actual worktree HEADs before merging.
- The harness auto-commits ("save N") and other sessions commit to `main`
  concurrently — fetch and reconcile before pushing.
- Agents occasionally edit the main checkout instead of their worktree — check
  `git status` in the main repo after a wave and revert stray edits.
- Apps directory is `.gitignore`d; `apps/TARGET_APPS.md` is the master list and
  pool snapshot (a working note, not version-controlled).
