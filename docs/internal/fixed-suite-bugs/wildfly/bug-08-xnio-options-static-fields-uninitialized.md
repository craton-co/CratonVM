# WildFly — `org.xnio.Options` static `Option` fields read as garbage (uninitialized `<clinit>` storage)

## Status
**FIXED** on `test/wildfly-suite` (`native-builtins/src/xnio_async.rs`, `ensure_options_initialized`). Verified: `OptAll` → `null=0` (was 69 null of 81); `XnioInit` → `OK loaded+initialized: org.xnio.XnioWorker` (was `ExceptionInInitializerError`).

## Root cause (final)
`Options.<clinit>` is **shimmed to a no-op** (registered native `native_options_clinit` at `xnio_async.rs`), so the stock clinit's `Option.simple(...)` cascade never runs. `ensure_options_initialized` then manually populated **only 12** of the 81 `public static final Option` fields — leaving 69 null, **including `WORKER_TASK_KEEPALIVE`**, which `XnioWorker.<clinit>` reads (`SetBuilder.add(Options.WORKER_TASK_KEEPALIVE)` → `add(null)` → `IllegalArgumentException`). It was NOT a generic static-field/class-init bug: `ManyStatics` (90 fields) and `ManyFinal` (static final) populate correctly; identical under JIT/`--nojit` and any heap size; `Options` is a real class (83 declared fields, not a synthetic stub); `Option.simple` real bytecode works (its native shim is already de-registered, `let _ = native_option_simple`).

## Fix
Extended the `specs` table in `ensure_options_initialized` from 12 to **all 83** `org.xnio.Options` option constants (name → value-type), so every `getstatic Options.<NAME>` observes a non-null synthetic Option. Keeps the existing synthetic-Option/OptionMap architecture intact. (Cosmetic residual: synthetic Option `toString()` is the bare-`Object` default rather than `org.xnio.Options.<NAME>` — pre-existing, non-functional; option identity for OptionMap keying uses the `OPT_DECLARING_CLASS`/`OPT_NAME` slots which are set correctly.)

## Severity
**CRITICAL** — single highest-blast-radius defect in the WildFly suite run (2026-06-18, dev binary @ `c4536b94`). Breaks WildFly remoting client bootstrap → cascades to ~21 of the first 31 results as `NoClassDefFoundError`/`ExceptionInInitializerError`, and is the likely cause of the `integration/basic` mass batch **hangs** (Arquillian management-client connect retries forever when remoting can't init).

## App / suite
- **Library:** XNIO `xnio-api-3.8.16.Final` (jboss remoting transport; on the classpath, *not* missing).
- **First observed failure:** `XnioWorker.<clinit>` during Arquillian remote-client init, across `integration/domain` (cli/suites) and `integration/basic` test classes.

## Symptom (as seen in the suite)

```
FAILCAUSE ... :: java.lang.NoClassDefFoundError: org/xnio/XnioWorker          (×13)
FAILCAUSE ... :: java.lang.ExceptionInInitializerError: null                  (×8, batch/EE tests)
```
First class to touch `XnioWorker` gets `ExceptionInInitializerError`; every later touch gets the cached `NoClassDefFoundError`.

## Root cause (confirmed by minimal repro)

`XnioWorker.<clinit>` (XnioWorker.java:859) does, in effect:
```java
OPTIONS = Option.setBuilder()
    .add(Options.WORKER_TASK_CORE_THREADS)   // <-- reads NULL
    .add(Options.WORKER_TASK_MAX_THREADS)
    .add(Options.WORKER_TASK_KEEPALIVE)
    .create();
```
`Option.SetBuilder.add(null)` correctly throws:
```
java.lang.IllegalArgumentException: Method parameter 'option' cannot be null
    at org.xnio.Option$SetBuilder.add(Option.java:265)
    at org.xnio.XnioWorker.<clinit>(XnioWorker.java:859)
```
→ wrapped as `ExceptionInInitializerError`. The real defect is upstream: **the static `Option` fields of `org.xnio.Options` are not correctly populated.**

### Repro (`repro/OptProbe2.java`, heap 256m)
```java
System.out.println("getstatic Options.ALLOW_BLOCKING = " + Options.ALLOW_BLOCKING);
System.out.println("getstatic Options.WORKER_TASK_CORE_THREADS = " + Options.WORKER_TASK_CORE_THREADS);
Option<Boolean> o = Option.simple(Options.class, "ALLOW_BLOCKING", Boolean.class);
System.out.println("direct Option.simple(...) = " + o);
```
CratonVM output (identical under JIT **and `--nojit`**):
```
getstatic Options.ALLOW_BLOCKING            = null
getstatic Options.WORKER_TASK_CORE_THREADS  = Object@70      <-- a bare java.lang.Object, not an Option
direct Option.simple(...)                   = org.xnio.Options.ALLOW_BLOCKING   <-- works fine
```

### What this tells us
- **`Option.simple()` is correct** — called directly it returns a proper `Option`.
- **The static fields hold garbage** — one reads `null`, another reads a bare `java.lang.Object` (wrong type for an `Option` field). It is also **non-deterministic**: under the transitive init path in `XnioInit`, `WORKER_TASK_CORE_THREADS` read as `null`; under the direct-getstatic path in `OptProbe2` it read as `Object@70`.
- **Not a JIT bug** — reproduces with `--nojit`, so it is an **interpreter-level** class-initialization / static-field-storage defect.
- `Class.forName("org.xnio.Options")` reports success, yet fields are unpopulated → `Options.<clinit>` is either **not actually executed**, or its `putstatic`/the later `getstatic` are hitting the **wrong static slots** (`Options` declares 83 static `Option` fields / 83 `putstatic` in `<clinit>`).

## HotSpot behavior
`getstatic Options.ALLOW_BLOCKING` triggers `Options.<clinit>`, which runs 83 `Option.simple(...)`/`putstatic` pairs; every field holds its `Option`. `XnioWorker.<clinit>` then builds its option set without error.

## Likely area / handoff
Interpreter static-field storage + class-init for classes with many static reference fields. Candidate angles for the next pass:
1. Confirm whether `org/xnio/Options.<clinit>` actually executes (add clinit-execution trace; the bare-`Object` garbage suggests static ref slots are **not zero-initialized** and `<clinit>` did not overwrite them).
2. Verify static-field slot assignment for `Options` (off-by-one / aliasing — reading field A returns field B's slot would explain null-vs-Object mix).
3. Check for any synthetic/native shadow of `org.xnio.Option`/`Options` that could mark the class "initialized" without running real bytecode (cf. native-shadowing family).

Thematically adjacent to the FIXED `Collections.emptyList()`-singleton bug (`bug-wildfly-collections-empty-singleton.md`), which was also "static fields populated by real `<clinit>` but not observed through the access path."

## Follow-on revealed by this fix (bug-09)
With XnioWorker now initializing, the cascade moves one class forward:
`NoClassDefFoundError: org/xnio/DefaultXnioWorkerHolder`. In-suite verification
of the bug-08 fix: domain `NoClassDefFoundError: XnioWorker` 13 → 0.

### bug-09 root cause (diagnosed)
`DefaultXnioWorkerHolder.<clinit>` → `OptionMap.create` → `option.cast(value)`:
```
AbstractMethodError: org/xnio/Option.cast(Ljava/lang/Object;)Ljava/lang/Object; has no Code attribute
    at org.xnio.OptionMap.create(OptionMap.java:165)
    at org.xnio.DefaultXnioWorkerHolder.<clinit>(DefaultXnioWorkerHolder.java:37)
```
The synthetic Options (from `ensure_options_initialized`) are instances of the
**abstract** `org/xnio/Option` class, whose `cast`/`parseValue`/`getName` are
abstract (no Code). The native `Option.cast` shim is **de-registered** (the
`xnio_async.rs` design comment: "`org.xnio.Option` is NOT synthesized" — real
`Option.simple` bytecode builds real `SingleOption`/`SequenceOption` with a real
`type` field, needed by WildFly `determineOptionType` reflection / WFLYSRV0055).

### bug-09 fix (supersedes bug-08's synthetic-completion)
Remove the `Options.<clinit>` native shim (`native_options_clinit`) registration
in `register_xnio_async_natives` (`xnio_async.rs`) so the **real** `Options.<clinit>`
runs, populating every field with a real Option (working `cast`, real `type`).
This subsumes bug-08 (real clinit sets all 81 fields) and fixes bug-09 (real
`cast`). The bug-08 83-entry synthetic `specs` table then becomes dead code
(`ensure_options_initialized` is no longer reached) and can be reverted.

**STATUS: FIXED + VERIFIED.** After freeing host disk (`wsl --shutdown` released
the WSL2 commit and the pagefile shrank), rebuilt and ran the probes:
- `OptAll` → `null=0 properOption=81 weird=0` (all 81 now REAL Options with
  proper `org.xnio.Options.<NAME>` toString — not synthetic);
- `XnioInit` → `OK loaded+initialized: org.xnio.XnioWorker`;
- `DXWH` → `OK loaded: org.xnio.DefaultXnioWorkerHolder`.

Cleanup left as a follow-up (functionally dead, never executed now): the bug-08
83-entry synthetic `specs` table in `ensure_options_initialized`, plus the
`Options.get<NAME>` accessor registrations, are dead and can be removed.

## Secondary defect observed (separate bug, file if reproducible)
During `OptProbe` (reflection path), cratonvm raised:
```
linkage error: no such method: java/lang/String.getName()Ljava/lang/String;
```
on the `java.lang.reflect.Field.get` / `Class.getField` path — a mis-resolved method dispatch unrelated to the Options storage bug.

## Repro assets
- `wildfly-suite/repro/OptProbe2.java` (+ `.class`) — minimal getstatic + `Option.simple` repro.
- `wildfly-suite/repro/XnioInit.java` — forces `XnioWorker.<clinit>`, dumps full cause chain.
- Classpath: `repro/` + `apps/wildfly/testsuite/domain/target/cratonvm-testcp.txt`.
- Run: `CRATONVM_DEFAULT_HEAP_MAX_MB=256 cratonvm --java-home <jdk25> @argfile OptProbe2` (works at low free disk; pagefile pre-grown).
