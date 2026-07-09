# TestAsyncContextImpl — `LogManager.getLogger()` returns null (NPE)

**Status:** OPEN - candidate VM fix landed; full Tomcat suite validation
pending. **Severity:** medium. **HotSpot:** PASS.

## Summary

`org.apache.catalina.core.TestAsyncContextImpl` fails `testAsyncIoEnd00` (and
sibling `testAsyncIoEnd01`, etc.) with:
```
1) testAsyncIoEnd00(org.apache.catalina.core.TestAsyncContextImpl)
java.lang.NullPointerException: Cannot invoke "java.util.logging.Logger.setLevel(java.util.logging.Level)"
  because the return value of "java.util.logging.LogManager.getLogger(String)" is null
```
`java.util.logging.LogManager.getLogger(String)` returns `null` for a logger
name that Tomcat's test setup expects to already exist (likely a
`java.util.logging.Logger.setLevel` call in test setup/teardown targeting a
named logger that should have been registered by JULI's
`ClassLoaderLogManager` or by a prior `Logger.getLogger(name)` call in the
same test). On HotSpot, `getLogger` finds the already-registered logger; on
CratonVM it comes back null, meaning either the logger was never registered
in CratonVM's `LogManager` backing store, or a per-classloader/JULI
registration path CratonVM handles differently loses the entry.

Found via a full Windows Tomcat suite rerun (real JDK, JIT on, 1500s
timeout, dev commit range `33bef88d`..`0d8fb610`, 2026-07-07/08).

## Reproduction

```powershell
cd C:\craton\CratonVM\apps\tomcat-suite-runner
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName asynclog `
  -Start <idx> -Count 1 -TimeoutSec 60 -Parallel 1
# org.apache.catalina.core.TestAsyncContextImpl
```

## Recommendation

Find the exact `Logger.setLevel`/`LogManager.getLogger` call site in
`TestAsyncContextImpl`'s test setup (likely a `@Before`/static initializer
silencing a noisy logger for async I/O tests) and trace which logger name is
being looked up. Check whether CratonVM's JULI/`LogManager` implementation
registers loggers eagerly (on first `Logger.getLogger()` call) vs lazily, and
whether a per-webapp-classloader `LogManager` (Tomcat uses
`org.apache.juli.ClassLoaderLogManager`, one instance per webapp classloader)
is being consulted correctly — a classloader mismatch between where the
logger was registered and where `getLogger` is later called would produce
exactly this null.

## Candidate fix evidence (2026-07-09)

Candidate root cause found in CratonVM's JUL LogManager native bridge:
`native-builtins/src/logmanager.rs::native_add_logger` read the logger name
from synthetic logger slot 0. That is only valid for CratonVM-created
synthetic `java.util.logging.Logger` objects. Real-JDK `Logger` instances,
including loggers passed through Tomcat JULI's `ClassLoaderLogManager`, keep
slot 0 as `Logger$ConfigurationData` and store the actual name in the real
`name` field. As a result, `addLogger(Logger)` could register the logger under
the wrong/empty name, so later `LogManager.getLogger(name)` did not observe the
JULI-registered logger that Tomcat expected.

Candidate fix:

- `native-builtins/src/logmanager.rs` now resolves logger names through a
  helper that first reads the real `name` field and only falls back to slot 0
  when slot 0 is actually a `java/lang/String` synthetic logger name.
- Added regression
  `logmanager::tests::tomcat0807_juli_add_logger_indexes_real_jdk_logger_name_field`,
  which models a Tomcat/JULI real-JDK logger with `config` in slot 0 and the
  logger name in the `name` field, then verifies a later
  `LogManager.getLogger(name)` returns the same registered logger.

Focused validation in worktree
`C:\craton\CratonVM-tomcat-0807-fixture-20260709-001`:

```powershell
rustfmt --check native-builtins\src\logmanager.rs
cargo test -p cratonvm-native-builtins tomcat0807_juli_add_logger_indexes_real_jdk_logger_name_field
cargo test -p cratonvm-native-builtins t19_h3_add_logger_returns_true_then_false_on_duplicate
cargo test -p cratonvm-native-builtins t19_h3_get_logger_is_idempotent_by_name
cargo test -p cratonvm-native-builtins logmanager::tests
```

Results: all targeted tests passed; `logmanager::tests` passed 28/28.

Remaining validation gap: this worktree does not contain `apps/tomcat` or
`apps/tomcat-suite-runner`, so the original
`org.apache.catalina.core.TestAsyncContextImpl` suite repro was not rerun here.
Keep this note open until the Tomcat runner verifies the async context class
with a binary containing the candidate fix. If that app-level run passes this
failure family, move the note to `docs/internal` or
`docs/internal/fixed-suite-bugs`.
