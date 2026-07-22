# `TestConnectionPool`/`TestDataSource` — "Wrong user name or password" is NOT the properties-sidetable cap bug — FIXED (as a side effect)

## Status
**FIXED** — confirmed 2026-07-22. This was split out of
`bug-h2-properties-sidetable-global-cap-silent-drop.md` (the properties
side-table cap bug, itself fixed via GC-aware weak-reference reclaim) as a
distinct, unidentified-root-cause finding. Direct re-verification against
the current `dev` tip shows both classes now pass reliably; the underlying
defect was resolved as a side effect of other fixes that landed on `dev` in
the same window, not by any change targeted at this bug specifically.

## Original symptom (for context)
Both `org.h2.test.jdbcx.TestConnectionPool` (`testPerformance`) and
`org.h2.test.jdbcx.TestDataSource` (`testDataSource`) failed with:
```
org.h2.jdbc.JdbcSQLInvalidAuthorizationSpecException: Wrong user name or password
	at org/h2/test/jdbcx/TestConnectionPool.testPerformance(TestConnectionPool.java:171)
	at java/sql/DriverManager.getConnection(DriverManager.java:199)
	...
	at org/h2/engine/Engine.validateUserAndPassword(Engine.java:396)
```
despite `Properties.put("user", ...)`/`put("password", ...)` on the
connection's info object succeeding immediately beforehand (confirmed via
temporary `CRATONVM_DIAG_PROPERTIES=1` instrumentation at the time of
filing) — a different failure shape from the properties-sidetable cap bug's
pure registration-time reject, hence filed separately. See the original
filing's evidence section (recovered from git history at
`e2d02ed21fc1f7921022897657fe13a31d23d1ef:docs/known-issues/h2-suite-bugs/bug-h2-connectionpool-datasource-wrong-password-not-cap.md`)
for the full original investigation.

## Verification (2026-07-22)
Bisected directly with two prebuilt binaries sharing the exact same H2
test classpath (`apps/h2database/h2`, `target/classes:target/test-classes:$(cat craton-testcp.txt)`,
`--java-home /home/victor/jdk25`):

| Binary | Commit | `TestConnectionPool` | `TestDataSource` |
|---|---|---|---|
| `cratonvm-propscap-20260722` (pre-merge, cap-fix only) | `a54444d93` | **FAILS** (matches original filing's stack trace exactly) | not separately re-run at this commit, but same code path |
| `cratonvm-propscap-postmerge-20260722` (post-merge, current `dev`) | `f5ac1d84c` and later (`6b3d1ebb`) | **PASSES** — 6/6 runs, JIT and `--nojit` | **PASSES** — 5/5 runs, JIT and `--nojit` |

This confirms the original filing's own claim (fix commit `a54444d93` alone
does not change either class's outcome) while also confirming something
else in the same `dev` window *does* fix it — the pre-merge binary
reliably reproduces the exact reported stack trace, and the current `dev`
tip does not, across repeated runs in both execution modes.

### Root cause attribution (not fully pinned down)
`properties_sidetable.rs` itself only changed once in the relevant range
(`a54444d93..f5ac1d84c`): `Properties.remove()` returning the real CHM's
removed value instead of discarding it for non-String values (part of
commit `3b03451b5`, "fix(jaas,properties): ..."). That change is about
`remove()`, not `put()`/`getProperty()`, and doesn't obviously explain this
symptom, so it likely isn't the actual fix.

The more plausible candidate, matching the original filing's own suspected
mechanism ("a race/ordering issue specific to this call-stack depth" /
"object-identity confusion"), is `a9ccfc37c` ("fix(vm): defer runnable
native root publication"), which changed when `safe_native_call` publishes
a thread's GC-root snapshot after a native call returns an object or throws
an exception — exactly the `Properties.put`-then-immediately-read pattern
this bug hit, several native-call frames deep
(`DriverManager.getConnection` -> `Driver.connect` -> `JdbcConnection.<init>`
-> `SessionRemote.connectEmbeddedOrServer` -> `Engine.createSession` ->
`Engine.validateUserAndPassword`).

This has not been isolated with a bisection build (each rebuild is
expensive and the host was under heavy concurrent load throughout this
investigation — see below); the fix is confirmed at the observable-behavior
level (the reported failure no longer reproduces), not attributed to a
single line of code. Left here as the most likely lead if this or a related
symptom resurfaces.

### Host conditions during this verification
The Azure build host was extremely loaded throughout this session (15+
concurrent WildFly/H2/other suite runs; system memory briefly fully
committed with no swap, and sshd stopped accepting connections for several
minutes). No fresh `cargo build` was performed for this doc closure — both
binaries used for the bisection above were already built by a prior
session's cap-fix investigation, at the exact commits cited. The pass/fail
split was reproduced identically across multiple repeats on both binaries,
so this is not attributed to host noise.

## Repro (for regression-checking; now expected to PASS)
```bash
cd apps/h2database/h2
<cratonvm-bin> --java-home <jdk25> \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.jdbcx.TestConnectionPool
<cratonvm-bin> --java-home <jdk25> \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.jdbcx.TestDataSource
```
