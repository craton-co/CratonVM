# FIXED — `java.util.logging.FileHandler`'s config-driven no-arg constructor is now instantiable in real-JDK mode

**Status: FIXED 2026-07-26.** Originally found 2026-07-26 while verifying
`docs/internal/fixed-suite-bugs/springboot/classutils-forname-platform-loader-false-positive.md`'s
residuals. Not part of that doc's ClassUtils/Console lineage — a separate,
unrelated bug that happens to live in the same test class
(`JavaLoggingSystemTests`). Closed the same day in a follow-up session
(worktree `wt-filehandler-20260726`, branch
`fix/filehandler-noarg-ctor-20260726`, Azure host) — see "Closure" at the
bottom.

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

## Why the obvious fix doesn't work (original investigation)

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
slots. This part of the original investigation's speculation was
**confirmed correct** — see Closure.

## Repro

```
core/spring-boot  org.springframework.boot.logging.java.JavaLoggingSystemTests
```
Run via `apps/spring-boot-suite-runner` or a direct `SbRunner` invocation
(see `docs/internal/fixed-suite-bugs/springboot/classutils-forname-platform-loader-false-positive.md`
for the ad-hoc classpath-fixup recipe used this session). Expect
`withFile` to fail with `Expecting actual not to be empty`; all other 11
tests in the class pass.

## Closure (2026-07-26): three independent, stacked bugs, all fixed

Re-investigated in worktree `wt-filehandler-20260726` (branch
`fix/filehandler-noarg-ctor-20260726`, from `origin/dev`, Azure host),
reusing the prior session's SbRunner classpath fixup at
`/data/data/tmp-classutils-residual/`. Confirmed `JavaLoggingSystemTests`
now passes **12/12** (multiple repeated runs, fully deterministic), and
found this needed **three** separate fixes stacked on top of each other —
the original doc's field-layout diagnosis was correct but was only the
first of three blockers actually hit, in order:

**Bug A — field-layout collision (as diagnosed above).** Confirmed via
`ctx.get_class(class_id).num_total_fields`-based allocation in
`new_object_initialized` (`vm/src/vm/vm_exec.rs`): for a real,
`java.base`-loaded class, this genuinely is the real declared-field count
including inherited fields, so raw numeric `ctx.set_field(this, N, ...)`
on `FileHandler` really does alias `Handler`'s real `manager`/`filter`/
`formatter` fields. **Fix**: replaced the raw 3-slot convention
(`filename=0, level=1, closed=2`) across `FileHandler`'s `<init>(String)`/
`<init>()`/`publish`/`close` natives with a GC-safe, identity-hash-keyed
side table (`jul_file_handler_state_table`, new in
`native-builtins/src/logging_shims.rs`) — the exact same pattern already
established for `Logger`'s handler list and filter (`jul_logger_handlers_table`/
`jul_logger_filters_table` in the same file, themselves fixing an
identical historical bug, see that code's own doc comments). Sidesteps
field layout entirely; immune to future real-JDK field-order changes.

**Bug B — dispatch-priority: real bytecode wins over the registered
native by default.** Even after adding a syntactically-correct
`<init>()V` native (calling it via
`ctx.new_object_initialized("java/util/logging/FileHandler", "()V", &[])`),
the test still failed identically — traced via targeted `eprintln!`
instrumentation to the REAL `FileHandler()` bytecode still running
instead of the native, throwing `java.nio.file.NoSuchFileException` on a
`spring.log.0.lck` lock file (real JUL's `openFiles()` trying to actually
open/lock a log file, which CratonVM's environment doesn't support the
way real JUL expects). Root cause: `vm/src/vm/vm_exec.rs`'s
`invoke_on_class_shared_inner` (the function `new_object_initialized`
actually calls) only overrides a CONCRETE (non-abstract) method's real
bytecode with a registered native when the `(class, method, descriptor)`
triple is on an explicit `check_override` allow-list — by default, real,
successfully-loaded bytecode always wins over a registered native for any
concrete method. `FileHandler`'s natives weren't on that list. **Fix**:
added `java/util/logging/FileHandler`'s `<init>()V`/`<init>(String)V`/
`publish`/`flush`/`close` to `check_override` in `vm_exec.rs`, and the
matching entry in `force_native_over_real_jdk_bytecode`
(`vm/src/runtime/interpreter.rs`, the twin gate for the interpreter's own
hot-loop dispatch) — see
[[cratonvm-real-switch-synthetic-stub-wins-by-default]]'s "three
independent dispatch mechanisms" note for why both gates were needed.

**Bug C — the native wasn't even registered in real-JDK mode at all.**
After fixing B, the exact same exception still reproduced — because
`register_p61_logging` (the function containing FileHandler's natives,
`native-builtins/src/phases_late.rs`) is called only from
`register_phase61_natives` → `register_synthetic_overrides`, which is
`#[cfg(feature = "synthetic-jdk")]`-gated and **never runs in real-JDK
mode** (the `--java-home` suite-runner default that Spring Boot's
`logging-file.properties` always exercises) — see
[[real-jdk-mode-registers-only-essential-natives]]. So the ENTIRE
FileHandler native surface (not just the no-arg ctor) had never actually
run in real-JDK mode, ever — the doc's own assumption that the
`<init>(String)`/`publish`/`flush`/`close` convention was "the existing,
working convention" was itself never actually exercised in the mode this
doc's repro uses; real bytecode had silently been running for ALL of
FileHandler's methods in real-JDK mode all along. **Fix**: split
FileHandler's natives out of `register_p61_logging` into their own `pub
fn register_p61_file_handler` (kept called from `register_p61_logging`
too, for synthetic-JDK-mode parity) and added an explicit call to it from
BOTH of `vm_init.rs`'s real-JDK-mode init arms, alongside
`register_essential_natives` — the same "individually promote a
phase-registration function into real mode" pattern already used there
for `register_concurrent_natives`/`register_stamped_lock_natives`.
Deliberately did NOT promote the rest of `register_p61_logging` (Logger's
`addHandler`/`setUseParentHandlers`/etc., `StreamHandler`'s own raw-slot
`<init>`) — those have their own, separate, never-independently-verified
raw-field-slot risk in real mode and are out of scope for this fix.

All three fixes were necessary; any one alone left the test failing
identically (confirmed by testing after each fix landed, before the next
was found).

**Regression check:** `cargo test --release -p cratonvm-native-builtins
--lib` — 5 pre-existing failures (2 in `logmanager::tests`, one each in
`cglib_enhancer`/`lang_string`/`regex_matcher`), all confirmed to fail
identically on the unmodified `dev` baseline (via `git stash`) — not
caused by this change. Also spot-checked
`LoggingApplicationListenerTests`/`LogbackLoggingSystemTests` on the same
ad-hoc classpath: both have pre-existing failures unrelated to FileHandler
(missing Logback XML test-resource files on this ad-hoc classpath, not
present in the JavaLoggingSystemTests-only classpath fixup used
originally) — confirmed via failure detail inspection, not just count.

Doc moved to `docs/internal/fixed-suite-bugs/springboot/` per the
known-issues-vs-internal convention (top status is now fully closed).
