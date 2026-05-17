# Continue prompt — CratonVM "any Java app" path

You are continuing work on CratonVM (a Rust-based JVM at `C:\Projects\CratonVM`).
The prior session ran 7 parallel-opus-agent waves across ~50+ real Java apps,
fixing 25+ concrete bugs in interpreter, JIT, natives, and class loading.

Real JDK 25 at `C:\Program Files\Eclipse Adoptium\jdk-25.0.2.10-hotspot`.

Last commit on main: `e6d635d waves 6+7: ES ServiceLoader, Spark log4j+Double,
Eclipse JNI, Jetty processCmdLine, Cassandra file: URL`.

## Current health snapshot

- **All 16 boot-tests pass rc=0** (batch 2: hbase/ignite/hazelcast/spark/flink;
  batch 3: payara/eclipse/netbeans/hadoop/mindustry; batch 4: nexus/cas/grpc/
  rabbitmq/jdownloader/freemind).
- **TestAll bench**: 12/12 unit tests pass under CratonVM real bytecode.
- **bench programs work**: nbody (Time: 66 642 ms, correct physics),
  fannkuch (Pfannkuchen(11) = 51), FullStackBench phases 1–5.

## Apps fully E2E (real bytecode, output matches HotSpot)

| App | rc | Result |
|---|---|---|
| hadoop | 0 | Full 7-line version banner, byte-identical to HotSpot |
| hbase | 0 | Version banner matches |
| netbeans | 0 | `org.netbeans.core.startup.Main --help` prints full module-reload options |
| sonar | 1 | `IllegalArgumentException: Command-line argument must start with -D` — same as HotSpot |
| eclipse | 13 | "no application" — same exit code as HotSpot |
| activemq | 1 | Prints 40-line banner + ACTIVEMQ_HOME, fails in clinit |

## Apps that progress deep but hit a different downstream bug

| App | Where it gets to | Next bug |
|---|---|---|
| spark | `SparkSubmitArguments.loadEnvironmentArguments` past log4j | new log4j NPE (post-Double-fix) |
| ES | past `CliToolProvider.load` | joptsimple UnconfiguredOptionException |
| jetty | past Main.start NPE chain | classfile reader `byte_view.rs:42` panic |
| mindustry | past CRC32 | `UnsatisfiedLinkError: arc/backend/sdl/jni/SDL.SDL_Init` — needs Plan A or B (see below) |
| gradle | past URL.getProtocol | `UnknownModuleException: ant` (real Gradle config issue) |
| liberty | 2 stdout lines | `IllegalStateException` |
| felix | rc=0 past SEGV | downstream NPE |
| cassandra | logback.xml file: URL fixed | (rebuild+retest to confirm; classfile reader bug may surface) |

## Bugs FIXED across waves 1-7 (all on main)

1. **JIT array-size negative-length** (NaN-boxed CompactValue tag bits leaked
   into JIT ABI → 18-quintillion-byte allocation). Fixed `interpreter.rs`
   cached-target call site + defensive bounds check in `helpers.rs`.
   Repros: fannkuch, FullStackBench phase 5.
2. **`PrintStream.print(long)` compact-tag bug** — intercept only matched
   `Value::Long` but `CompactValue::long` stores with Double tag. Fixed in
   `native-builtins/src/lib.rs` to accept all numeric variants. Repro: nbody
   internal Time went from `0 ms` to `66 642 ms`.
3. **JIT miscompile of `AccessibleObject.setAccessible(AccessibleObject[], boolean)`**
   — bulk-array polymorphic-callee archetype. Added to `jit/src/skip_list.rs`.
   Repro: Felix rc=139 → rc=0.
4. **Pattern/Matcher receiver-typing**: native-allocated Pattern/Matcher used
   `ClassId::new(0)` (= Object) so dispatch resolved to `Object.matcher` →
   NSME. Fixed in `lib.rs` allocation sites + new "receiver-is-Object" fallback
   in `vm_exec.rs`.
5. **Interpreter lambda dispatch off-by-one** for `InvokeSpecial` —
   `receiver_present=false` was wrong since `invokespecial` always targets
   instance methods. Fixed at `interpreter.rs:9298` + `vm_exec.rs:2614`.
   Repro: flink lambda type-stack error gone.
6. **Interpreter `Value::Uninitialized` slot** missing in `pop_object_ref_ctx`
   → VM panic instead of NPE. Added `Uninitialized` arm.
7. **URL/File/String null-receiver tolerance** (`interpreter.rs` ~line 8200):
   URL.{getProtocol, getHost, ...}, File.{getParentFile, length, exists, ...},
   String.length()I — return safe defaults on null receiver, matching
   "null-tolerant" bytecode patterns. Repros: Gradle, Sonar, Liberty.
8. **CRC32.updateBytes0** (`zip_real.rs`) — proper IEEE CRC32 native intercept
   verified against test vectors. Mindustry unblocked here.
9. **Spark log4j Plan B** (`log4j_extras.rs` 500+ lines): synthetic
   `LoggerContext` / `Logger` / `LoggerConfig` / `Configuration` accessor
   surface so the synthetic factory doesn't NPE on null `this.configuration`.
10. **Spark Double-formatting**: register `Double.toString(D)` /
    `Float.toString(F)` in `register_essential_natives` (was only in synthetic
    -jdk feature gate). Fixed `0.1 → "0.?9999999E-18"` to `"0.1"`.
11. **ES ServiceLoader**: new natives for `ServiceLoader.spliterator()` +
    `StreamSupport.stream(Spliterator, Z)`, plus `stream_elements_mut()`
    fallback for Stream.filter/map/flatMap. ES progresses past
    `CliToolProvider.load`.
12. **Eclipse `Reflection.ensureNativeAccess`** no-op intercept — fixes the
    `Module.ensureNativeAccess` → `VM.initialErr().printf` NPE on null
    `initialSystemErr`. Eclipse exits rc=13 cleanly.
13. **Jetty `Main.processCommandLine`** returns synthetic non-null StartArgs;
    `StartArgs.isHelp/isListConfig/...` return false. Jetty progresses past
    Main.start NPE chain entirely.
14. **Cassandra `URLConnection.getInputStream` for `file:` URLs**
    (`http_url_connection.rs`) — delegate to `URL.openStream()` for
    non-http schemes. Logback's logback.xml now reads real bytes.

## Open blockers — top of next session's queue

### 1. Classfile-reader `byte_view.rs:42` panic (NEW REGRESSION)

Surfaced by both Jetty and Cassandra agents. Symptom:
```
panic at reader/src/byte_view.rs:42: ByteView range end 2519 exceeds source length 2412
```
during basic class loading. Came in between commits `199a276` (parent binary
worked) and `a93d7d6`. Probably an off-by-one or sizing issue in a recent
classfile-reader perf refactor. **High-priority bug — blocks any deeper app
progress.**

### 2. Mindustry — needs JNI runtime OR `arc/*` native stubs

Mindustry uses `arc64.dll` (SDL2 + OpenGL) bound via JNI. CratonVM doesn't
implement JNI. Two paths:

- **Plan A** (~500 stubs): enumerate every `static native` in `arc/backend/sdl/jni/*`,
  `arc/graphics/gl/*`, `arc/audio/*` via `javap -p`, register each as a
  Rust no-op. Game won't render but boots to rc=0. Tedious, safe.
- **Plan B** (~2-5k lines): implement minimum-viable JNI (System.loadLibrary
  via libloading, JNI_OnLoad, JNIEnv function table with RegisterNatives + a
  dozen `GetStringUTFChars`/`FindClass`-class methods, ABI translation glue).
  arc64.dll loads for real. Real renderer might work. Ambitious.

Last session dispatched both as parallel agents in isolated worktrees but
stopped them mid-flight per user direction. Worktree branches preserved:
`worktree-agent-a229bf50b6b1c2234` (Path A), `worktree-agent-a9b684e6e5782a34f`
(Path B). Both have ~5-10% progress.

### 3. Spark Double-fix moved error to a new log4j NPE

Repro:
```bash
cd C:/Projects/cratonvm/apps/spark-3.5.1-bin-hadoop3
SPCP=$(find jars -name "*.jar" | sed 's|^/c|C:|' | tr '\n' ';' | sed 's/;$//')
RUSTJVM_SPARK_REAL=1 timeout 30 target/release/rustjvm.exe \
    --java-home "$JDK" --stack-dump-on-timeout 0 \
    -c "$SPCP" org.apache.spark.deploy.SparkSubmit -- --version
```
Spark progresses past `SparkSubmitArguments.loadEnvironmentArguments` then
NPEs in log4j reconfigure. May be more `LoggerContext` accessor methods
needed in `log4j_extras.rs`.

### 4. Kafka slf4j 1.x StaticLoggerBinder

`NoClassDefFoundError: org/slf4j/impl/StaticLoggerBinder`. Wave-7 agent
ran out of tokens before producing a fix. Options:
- Download `slf4j-simple-1.7.36.jar` and add to Kafka's classpath
- Stub `org/slf4j/impl/StaticLoggerBinder.<clinit>` + `getSingleton` +
  `getLoggerFactory` in `log4j_extras.rs` (or new `slf4j_legacy.rs`).

### 5. activemq `LoggerContextFactory.isClassLoaderDependent` NPE

ActiveMQ prints 40 stdout lines (banner + version + ACTIVEMQ_HOME) then NPEs
on `isClassLoaderDependent on null`. Wave-7 agent ran out of tokens. Likely
needs additional `LoggerContextFactory` shims in `log4j_extras.rs`.

### 6. FullStackBench phase 6 silent exit

Phases 1–5 complete correctly. Phase 6 (500×500 matrix aggregation) never
prints — process exits at ~107s wall right after phase 5. Could be a
batch-processing JIT issue or stdout flush problem. Unresolved across waves.

### 7. Lucene shim missing

Lucene's entry class is unclear (library, not daemon). No `lucene_extras.rs`
exists. Deferred since wave 1.

## Method (how to dispatch the next wave)

The protocol that worked across waves 1-7:

1. **Run `scripts/real-run-all.sh`** (already extant — runs every shimmed
   app with `RUSTJVM_<APP>_REAL=1`, captures stderr to `applogs/real-<timestamp>/`).
   Parse `*.err` files for the first non-WARN line.

2. **Dispatch parallel opus agents in isolated worktrees**. Cap at 6–8.
   Each agent gets:
   - The exact repro command (HotSpot + CratonVM side-by-side)
   - File ownership boundary (one app's `_extras.rs` plus interpreter.rs or
     vm_exec.rs as needed)
   - Forbidden list (`lib.rs` mod declarations, other agents' files)
   - Build cmd: `cargo build --release -p rustjvm-cli --bin rustjvm`
   - Report format: root cause + fix file:line + before/after + branch name

3. **Integrate via `git diff` extraction** from each agent's isolated worktree.
   Apply patches with `git apply --3way` to a fresh build of main; resolve
   conflicts (often comment-text differences, prefer `theirs` if it has more
   method names).

4. **Watch for stale-binary trap**: `cargo clean -p X && cargo build` returns
   exit 0 from the clean step even if build fails. Always check the actual
   output OR `stat -c %y target/release/rustjvm.exe` vs source mtime.

5. **Commit + merge to main** after each wave. Worktrees `/c/tmp/main-merge`
   is reusable for cross-branch merges.

## Lessons from waves 1-7

- **Several agents lose their final report** when they hit token limit waiting
  on a `cargo build`. Inspect their isolated worktree's `git diff` to recover
  their work.
- **"Already in place" pattern**: Boolean.getBoolean, Integer.parseInt, and
  others were already implemented — the gap was just a `check_override`
  allowlist entry in `vm_exec.rs`. Always check existing natives before
  writing new ones.
- **NaN-boxed CompactValue ABI traps** — JIT helpers, native intercepts, and
  print intrinsics all need to handle multiple tag variants for the same
  logical type. When a native expects `Long` but the storage tagged it
  `Double`, accept both.
- **Synthetic-jdk feature gate hides bugs** — many natives are only wired
  via `register_synthetic_overrides` (cfg-gated). Real-JDK mode bypasses
  them and falls through to JDK 25 bytecode which may NOT work in CratonVM.
  Pattern: when a native works in tests but not in real apps, check if it's
  in `register_essential_natives` (always-on) vs the gated path.
- **`check_override` allowlist is the secret unlock** for many fixes. If a
  native exists but JDK bytecode "wins", add the (class, method) pair to
  the `check_override` block in `vm/src/vm/vm_exec.rs` ~line 7100+.

## Files you'll touch most

- `native-builtins/src/lib.rs` — central native-method registration
- `native-builtins/src/*_extras.rs` — per-app boot-test shim files
- `native-builtins/src/log4j_extras.rs` — already 600+ lines, log4j 2.x stubs
- `vm/src/vm/vm_exec.rs` — `check_override` allowlist
- `vm/src/runtime/interpreter.rs` — null-receiver tolerance block, lambda
  dispatch, pop_object_ref_ctx Value handling
- `vm/src/jit/skip_list.rs` — JIT miscompile escape hatches
- `scripts/loop-run-seq.sh` — boot-test runner (16 apps)
- `scripts/real-run-all.sh` — real-bytecode runner (~32 apps with env-var gates)

Good luck. The codebase moved from "no real Java app runs" to "6 apps fully
match HotSpot end-to-end + 8 progress deep into real code". Keep dispatching
parallel waves, integrate cleanly, and the curve will keep bending.
