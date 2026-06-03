# Continue: H2 `org.h2.test.TestAll` — residual interpreter bug + EC-slow perf (after the small fixes land)

**Severity:** medium-high — H2 is a target app; basic JDBC already works, individual tests are unblocked by the small StackWalker/toArray fixes, but `TestAll` still doesn't complete green. Self-contained session.

## State / repro
H2 is **already built** at `apps/h2database/h2` (NOT `_test-suites/`); test classes are in `temp/` (1703 `.class`). Classpath:
```
cd apps/h2database/h2
CP='temp;ext/jts-core-1.19.0.jar;ext/jakarta.servlet-api-5.0.0.jar;ext/javax.servlet-api-4.0.1.jar;ext/asm-9.5.jar;ext/lucene-core-9.7.0.jar;ext/lucene-analysis-common-9.7.0.jar;ext/lucene-queryparser-9.7.0.jar;ext/slf4j-api-2.0.7.jar;ext/junit-jupiter-api-5.10.0.jar;ext/apiguardian-1.1.2.jar;ext/org.osgi.core-5.0.0.jar;ext/org.osgi.service.jdbc-1.1.0.jar'
target/release/cratonvm.exe --java-home "C:/Program Files/Java/jdk-25" --stack-dump-on-timeout 0 -Xmx1g -cp "$CP" org.h2.test.TestAll   # bound with `timeout 240`; HotSpot ~133s
```
- **Basic JDBC works** (a `getConnection("jdbc:h2:mem:test;USER=sa")` + CREATE/INSERT/SELECT probe prints `OK` on both VMs; the `Properties.remove`/USER fix `2045ff5` holds).

## Prerequisites that should land FIRST (small fixes, possibly already committed by the session that wrote this)
1. **`StackWalker.getCallerClass()` off-by-one** — blocks every single-test entrypoint (`TestX.main` → `TestBase.createCaller()` → `getCallerClass`). `native-builtins/src/stack_walker.rs:203` (and dup `native-builtins/src/phases_late.rs:15066`) return the `@CallerSensitive` caller's class (`TestBase`) instead of *its* caller (`TestDate`). **Caveat:** JIT vs `--nojit` produce different `thread.frames` shapes (`--nojit` already returns correct), so verify both modes — a blind "skip one more" may over-skip under `--nojit`.
2. **`Collection.toArray(T[])` multi-dim** — `native-collections/src/lib.rs:1264` allocates a bare `Object[]` when the template is too small, dropping the component type. H2-free repro: `List<String[]>.toArray(new String[0][])` → CCE `Object -> [[String`. Fix: allocate with the template's runtime component type (needs a component-class accessor — `class_id_of_object(template)` gives the array class; add a way to get its component ClassId, or a name→ClassId resolver). H2 hits this in `org.h2.result.SortOrder.sort` (`Value[][]`).

## The residual TestAll failures (this session's real work)
With JDBC + single tests working, `TestAll` still times out (vs HotSpot ~133s). Three distinct CratonVM-only signatures (verified absent from the HotSpot baseline):
1. **`ClassCastException: java/lang/Object cannot be cast to [[Lorg/h2/value/Value;`** (×7) from `org.h2.result.SortOrder.sort` (SortOrder.java:220/213) — this is the `toArray(T[])` multi-dim bug above; the small fix should clear it.
2. **`IllegalStateException: operand stack underflow`** (many, near the end) — an **interpreter/verifier bug** on some H2 SQL-execution bytecode path. **Not yet minimized.** This is the main remaining work: bisect which H2 test / SQL script triggers it, minimize to a small method, and root-cause the operand-stack tracking in `vm/src/runtime/interpreter.rs` (look at the opcode whose stack effect is mis-modelled — likely a dup/pop variant, a wide form, or an exception-handler stack-reset path).
3. Expected negative-test noise (e.g. "Data conversion error converting Hello") that also appears on HotSpot — ignore.

## Plus: EC/BigInteger interpreter perf (shared with BC math.ec)
`TestAll` (and BC `math.ec`) expose that the interpreter is ~40-60× HotSpot on EC/object-churn glue. Not a correctness bug, but it makes long suites time out. If TestAll still overruns after the correctness fixes, profile the hot interpreted paths (object allocation per op, method-dispatch overhead) and/or ensure the hot methods tier into JIT. Lower priority than the underflow.

## Plan
1. Land the two small fixes (StackWalker, toArray) if not already; confirm single H2 tests (`org.h2.test.unit.TestDate`) pass.
2. Minimize the `operand stack underflow`: run TestAll with `--stack-dump-on-timeout` to capture frames; bisect the H2 sub-suite (TestAll has a `-test` selector / runs `addTest`-registered classes) to the offending class, then to a single SQL script (`org/h2/test/scripts/*.sql` via `TestScript`), then to a minimal Java method. Root-cause the interpreter stack accounting.
3. Re-run `TestAll` bounded; target: completes within a few× HotSpot with no CratonVM-only ClassCast/underflow. (HotSpot itself has 4 upstream FAILs — TestFunctions/TestPreparedStatement/TestCrashAPI/TestOutOfMemory — JDK25 incompats, not CratonVM bugs.)

## Key files
`native-builtins/src/stack_walker.rs:203`, `native-builtins/src/phases_late.rs:15066` (StackWalker dup), `native-collections/src/lib.rs:1264` (toArray), `vm/src/runtime/interpreter.rs` (operand-stack tracking / opcode stack effects). H2 tree `apps/h2database/h2`. HotSpot baseline 133s, 4 upstream-only FAILs.
