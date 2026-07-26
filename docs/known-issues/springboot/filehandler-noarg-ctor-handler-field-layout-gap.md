# OPEN — `java.util.logging.FileHandler`'s config-driven no-arg constructor is uninstantiable, and a naive native fix risks corrupting real `Handler` fields

**Status: OPEN — found 2026-07-26** while verifying
`docs/internal/fixed-suite-bugs/springboot/classutils-forname-platform-loader-false-positive.md`'s
residuals. Not part of that doc's ClassUtils/Console lineage — a separate,
unrelated bug that happens to live in the same test class
(`JavaLoggingSystemTests`).

## Symptom

`JavaLoggingSystemTests.withFile(CapturedOutput)` fails:
`Expecting actual not to be empty` — asserting that
`tmpDir().listFiles(SPRING_LOG_FILTER)` is non-empty after
`JavaLoggingSystem.initialize(context, null, getLogFile(null, tmpDir()))`
and a subsequent `logger.info(...)`. No exception surfaces anywhere in the
test output; the `spring.log` file is simply never created. Confirmed
**not** a regression: identical failure on dev before and after this
session's `java.io.Console.ttyStatus()` fix (see the classutils doc above).
Confirmed **not** an environment/tmpdir artifact: real JDK 25, run on the
same host against the same classpath with the same `-Djava.io.tmpdir`
override, passes this test (and the whole class) 12/12.

## Root cause (confirmed via debug instrumentation)

`org.springframework.boot.logging.java.JavaLoggingSystem.loadConfiguration`
calls `LogManager.getLogManager().readConfiguration(InputStream)` with
Spring Boot's `logging-file.properties` (its `${LOG_FILE}` placeholder
already substituted with a real absolute path by Spring Boot's own Java
code). That native override
(`native_read_configuration_with_stream` → `apply_jul_config_entries` in
`native-builtins/src/logmanager.rs`) parses `handlers=...` and
instantiates each listed handler class generically via
`ctx.new_object_initialized(cls, "()V", &[])` — a **no-arg** constructor
call, for every class listed.

`java/util/logging/FileHandler`'s only native constructor override
(`native-builtins/src/phases_late.rs`, `register_p61_logging`) is
`<init>(Ljava/lang/String;)V` — there is no native (or apparently working
real-bytecode) `<init>()V`. `new_object_initialized(cls, "()V", &[])`
therefore returns `Ok(None)`/`Err(...)` for `FileHandler`, and
`apply_jul_config_entries`'s `if let Ok(Some(...)) = ...` silently treats
it as "handler class missing/uninstantiable — skip it" (a deliberate,
documented fallback for genuinely-missing classes, not for this case).
Only the `ConsoleHandler` (which *does* have a working no-arg path)
actually gets attached to the root logger, so file output never happens.

## Why the obvious fix doesn't work

A no-arg `FileHandler.<init>()V` native, added and verified via debug
`eprintln!` to correctly resolve `java.util.logging.FileHandler.pattern`
from the same `parsed_log_properties()` side-table that
`native_get_property` already exposes to real bytecode, still did not fix
the test — the field write appeared to succeed, but no file appeared and
no exception surfaced.

The existing `<init>(Ljava/lang/String;)V`/`publish`/`flush`/`close`
native overrides use a synthetic 3-field convention
(`filename=0, level=1, closed=2`) that predates this investigation. But
`java.util.logging.Handler` (an ancestor of `FileHandler` via
`StreamHandler`) is a **real** class with its own declared instance
fields, in this declaration order (JDK 25, `javap -p`):

```
private final java.util.logging.LogManager manager;   // slot 0?
private volatile java.util.logging.Filter filter;      // slot 1?
private volatile java.util.logging.Formatter formatter;// slot 2?
private volatile java.util.logging.Level logLevel;     // slot 3?
private volatile java.util.logging.ErrorManager errorManager;
private volatile java.lang.String encoding;
```

If CratonVM's `ctx.set_field(this, N, ...)` for a **real**, bytecode-backed
object indexes by real declared-field order (as opposed to a private
native-only shadow-field side table used only for fully-synthetic
classes), then the existing `filename=0/level=1/closed=2` convention
almost certainly collides with the real `manager`/`filter`/`formatter`
slots — writing a `String` into what bytecode expects to be a
`LogManager` reference, `null` into `filter` (probably harmless, matches
the real default), and an `Int(0)` into what bytecode expects to be a
`Formatter` reference (**not** harmless: any later real-bytecode path
that reads `this.formatter` would see an `Int` where it expects an
object reference). Separately, no code path ever runs the real
`Handler()` super-constructor (native override entirely replaces
`<init>`), so `logLevel`/`errorManager` are never initialized at all —
`isLoggable()` (real, uninterrupted bytecode, inherited from `Handler`
and never overridden) calling `getLevel().intValue()` on a null
`logLevel` would `NullPointerException`, most likely inside JUL's own
internal per-handler dispatch try/catch, explaining why nothing ever
propagates up to the test but the file is still never written.

This is unconfirmed speculation about the exact failure point (whether
it's the level-NPE swallowed by JUL, or a `formatter` type-confusion
crash, or something else) — it needs actual verification (e.g. a debug
build with `RUST_BACKTRACE`/panic hooks around `ctx.set_field`/
`get_field`, or instrumenting `isLoggable`/`getLevel` call sites) before
attempting a real fix. The speculative no-arg-ctor native written during
this investigation was reverted rather than shipped half-understood — it
changed nothing observable (test still fails identically) but risked
writing wrong-typed values into real inherited fields for zero benefit.

## What a real fix likely needs

Understand CratonVM's field-indexing convention for `ctx.set_field`/
`get_field` when called from a native override on a **real**
(non-synthetic) class — does index `N` mean "the Nth field in
real-bytecode declared order including inherited fields", or a private
native-only side table? If the former, either:
- Extend the constructor override to also invoke the real `setLevel`/
  `setFormatter`/`setFilter`/`setErrorManager` **virtual** methods (real
  bytecode, not overridden) with correctly-typed real objects, so
  `Handler`'s real inherited fields end up valid — and use *additional*,
  higher-numbered field slots (past `FileHandler`'s own real declared
  fields) for the synthetic `filename`/`closed` bookkeeping instead of
  slots 0-2; or
- Have `apply_jul_config_entries` call `ctx.invoke_special_bytecode_only`
  (already used elsewhere, e.g. `ClassUtils.isPresent`) for the no-arg
  constructor instead of relying on a from-scratch native, letting the
  real `Handler()`/`StreamHandler()` super-chain run so all inherited
  fields initialize normally, then patch in the filename afterward.

## Repro

```
core/spring-boot  org.springframework.boot.logging.java.JavaLoggingSystemTests
```
Run via `apps/spring-boot-suite-runner` or a direct `SbRunner` invocation
(see `docs/internal/fixed-suite-bugs/springboot/classutils-forname-platform-loader-false-positive.md`
for the ad-hoc classpath-fixup recipe used this session). Expect
`withFile` to fail with `Expecting actual not to be empty`; all other 11
tests in the class pass.
