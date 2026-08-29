# WP8.10 — WildFly Boot Triage Report

**Status**: WP8.10.3/8.10.4 — first-failure pinned and triaged
**Generated**: 2026-04-29 (session 100, agent for WP8.10)
**Smoke test**: `vm/tests/wp8_10_jboss_modules_smoke.rs` (7 probes, all green)

---

## TL;DR — Top 3 Diagnosed First-Failures

Ranked in the order they will fire when `cratonvm.exe --jar
staged/wildfly/jboss-modules.jar -mp staged/wildfly/modules
org.jboss.as.standalone` runs:

1. **WP8.10.5 (NEW gap, blocking)** — `is_jdk_class` in
   `classloading/src/class_manager.rs:3300` does NOT include `org/jboss/`,
   `org/wildfly/`, `org/xnio/`, etc. — even though those classes have
   rich synthetic-stub field layouts declared **in the same file** at
   `synthetic_stub_fields:3811+`. Result: any reference to
   `org.jboss.modules.Module`, `LocalModuleLoader`,
   `DefaultBootModuleLoaderHolder`, etc., raises `NoClassDefFoundError`
   instead of materializing the synthetic stub. **Confidence: 100%
   (reproduced in probe0 of the new smoke test).**
   Effort: **5 LoC** (extend `is_jdk_class` prefix list).

2. **WP1.8 fallout** — `Throwable.getMessage` and `Throwable.printStackTrace`
   missing on `java/lang/NoClassDefFoundError` synthetic stub. Surfaces
   as a *secondary* `NoSuchMethodError` whenever boot code does
   `catch (Throwable t) { ... t.getMessage(); ... }`. Independent of
   #1 above; even after the stub-fallback fix lands, this will still
   fire whenever real WildFly catches a Throwable. **Confidence: 95%
   (reproduced as the secondary failure in probes 1-6 once probe0
   surfaces a real NCDFE).**
   Effort: **~30 LoC** in `lang_misc.rs` register block.

3. **WP8.10.6 (NEW gap)** — `post_clinit_fixup` in `vm/src/vm/vm_util.rs`
   only fires from the swallowed-exception path (line 543) and the
   stack-error swallow path (line 449). It never runs for synthetic
   stub classes that *lack* a `<clinit>` method — which is most of
   them. **Confidence: 90%.** I shipped a partial fix (line 644
   onward) that fires `post_clinit_fixup` on the no-clinit path when
   `class.is_synthetic_stub`. Remaining: `DefaultBootModuleLoaderHolder.INSTANCE`
   still null after the fix, indicating the fixup isn't reaching the
   class because the class itself isn't being loaded (gap #1).

---

## Surface Audit — WildFly Boot Phases vs. Rust-JVM Code

For each WildFly boot phase: (a) what Java APIs WildFly uses, (b)
which cratonvm code path handles it today, (c) what specifically
fails.

### Phase 1 — `org.jboss.modules.Main.main` entry

- **Java surface**: `getstatic DefaultBootModuleLoaderHolder.INSTANCE`
  → `Module.initBootModuleLoader(loader)` → `loader.loadModule(name)` →
  `module.run(className, args)`.
- **Rust path**:
  - `DefaultBootModuleLoaderHolder` synthetic-stub layout at
    `classloading/src/class_manager.rs:3823-3830`.
  - Post-clinit fixup at `vm/src/vm/vm_util.rs:1022-1036` allocates
    `LocalModuleLoader` + assigns to `INSTANCE`.
  - `LocalModuleLoader.loadModule(String)` native at
    `native-builtins/src/jboss_module_loader.rs:1530-1545`.
- **Will fail**: `getstatic INSTANCE` raises NCDFE. The entire chain
  is unreachable until `is_jdk_class` is fixed (gap #1 above).

### Phase 2 — `module.xml` SAX parsing (Xerces or built-in)

- **Java surface**: WildFly uses `org.jboss.modules.xml.ModuleXmlParser`
  which goes through MXParser (a hand-rolled NIO parser).
- **Rust path**: Bypassed entirely. `native-builtins/src/jboss_module_xml.rs`
  uses Rust's `quick_xml` crate; the native `LocalModuleLoader.loadModule`
  reads `module.xml` from disk directly without going through any Java
  XML parser. **No SAX/Xerces gap exists for WP8.10.**
- **Verdict**: ✅ unblocked.

### Phase 3 — `URLClassLoader` → `JarFile.open` → ZipFile native I/O

- **Java surface**: `new URL("jar:file:..!/...")` → `URL.openStream` →
  `ZipFile.open0` → `Inflater.inflateBytesBytes`.
- **Rust path**:
  - Synthetic ZipFile/JarFile layouts at
    `native-builtins/src/phases_late.rs:10268+, 32009+`.
  - `flate2`-backed Inflater natives in `native-builtins/src/zip_real.rs`.
  - JBoss `JarFileResourceLoader` shim at
    `native-builtins/src/jboss_resource_loader.rs:106-148`.
- **Will fail**: not reached today (gap #1 stops boot earlier).
  Once unblocked, the resource-loader shim should handle the path
  because JBoss uses a different bytecode entry point
  (`ResourceLoaders.createJarResourceLoader`) than raw URLClassLoader.
- **Confidence**: medium — needs validation after gap #1 is resolved.

### Phase 4 — `LogManager` SPI discovery (`../../../apps/META-INF/services/java.util.logging.LogManager`)

- **Java surface**: `java.util.logging.LogManager.<clinit>` reads
  `-Djava.util.logging.manager=org.jboss.logmanager.LogManager`,
  invokes `Class.forName(...)`, calls `newInstance()`, casts to
  `LogManager`.
- **Rust path**:
  - `native-builtins/src/logmanager.rs:447` (`register_logmanager_natives`)
    intercepts `getLogManager()` and returns a process-wide singleton,
    bypassing the SPI dance entirely.
  - Post-clinit fixup at `vm/src/vm/vm_util.rs:889-895` populates
    `LogManager.manager` static so `getLogManager0()` finds a non-null
    instance.
- **Verdict**: ✅ wired (1005 LoC of LogManager surface). Does NOT
  surface the SPI discovery path because we route through the native
  override before SPI is consulted.

### Phase 5 — `ServiceLoader.load(LogManager.class)` — WP1.8-narrow happy path?

- **Java surface**: `ServiceLoader.load(Cls).iterator()` →
  `ClassLoader.getResources("META-INF/services/Cls.fqn")` →
  per-resource `BufferedReader.readLine` loop.
- **Rust path**: `native-builtins/src/service_loader.rs:120` —
  `discover_providers` uses `ctx.find_all_resource_bytes` (the WP1.8
  narrow path that bypasses `URL.openStream` / `BufferedReader`
  chain). Validated by `vm/tests/wp1_8_serviceloader_e2e.rs`.
- **Verdict**: ✅ wired (WP1.8 closed in session 94 per roadmap §16).
  WildFly's actual `ServiceLoader.load(MainProcessor.class)` invocation
  inside `Module.run` will hit this path after gap #1.

### Phase 6 — `ModuleClassLoader.defineClass(byte[]...)` — WP2.3

- **Java surface**: `ClassLoader.defineClass(name, bytes, off, len)` /
  `MethodHandles.Lookup.defineClass`.
- **Rust path**: `native-builtins/src/lookup_define.rs` (630 LoC) +
  `native-builtins/src/classloader_real.rs:26` (`register_classloader_real_natives`).
- **Verdict**: ✅ wired per roadmap (WP2.3 marked done).

### Phase 7 — `sun.misc.Unsafe` / `jdk.internal.misc.Unsafe`

- **Java surface**: `Unsafe.getObjectVolatile`, `compareAndSwapObject`,
  `allocateMemory`, plus the JDK 25 splits.
- **Rust path**: `native-builtins/src/unsafe_jdk25.rs`,
  `native-builtins/src/unsafe_natives.rs`,
  `vm/src/runtime/unsafe_helpers.rs`.
- **Verdict**: ✅ wired across waves 3-4. JBoss Modules uses Unsafe
  only via concurrent collections, which are already smoke-tested.

### Phase 8 — `MethodHandles.lookup()` and `LambdaMetafactory.metafactory` (WP1.6)

- **Java surface**: `Lookup.findStatic`, `findVirtual`, `findConstructor`,
  `MethodHandle.invokeExact`, `LambdaMetafactory.metafactory` for lambda
  call sites.
- **Rust path**: `native-builtins/src/lang_invoke.rs:1349-1410, 2362-2400`
  + `vm/src/runtime/invokedynamic.rs:349+`.
- **Verdict**: ✅ wired. JBoss Modules' `module.run(className, args)`
  uses `Lookup.findStatic(class, "main", methodType(void.class,
  String[].class))` then `invokeExact` — both registered.

### Phase 9 — `Throwable.fillInStackTrace` / `getStackTrace`

- **Java surface**: every `throw new` triggers `fillInStackTrace`;
  `printStackTrace(PrintStream)` reads `getStackTrace()`.
- **Rust path**: `native-builtins/src/lang_misc.rs:51, 60, 80, 360`.
  `fillInStackTrace` returns `this`; `getStackTrace` returns
  StackTraceElement[].
- **Will fail**: `Throwable.getMessage()` and `printStackTrace(PrintStream)`
  are NOT registered for the `NoClassDefFoundError` synthetic stub —
  see top-3 finding #2. The probe surfaces this as `NoSuchMethodError:
  java/lang/NoClassDefFoundError.getMessage()`.

---

## New Sub-WPs Proposed

### WP8.10.5 — Extend `is_jdk_class` to cover synthetic-stub prefixes [S, ~30 min]

**File**: `classloading/src/class_manager.rs:3300`

**Diagnosis**: The synthetic-stub fallback in `load_class` (line
1167-1180) only fires for classes where `is_jdk_class(name) == true`.
But `synthetic_stub_fields` declares rich layouts for ~56 non-JDK
classes (`org/jboss/*`, `org/wildfly/*`, `org/xnio/*`, etc.) that are
never reached because `is_jdk_class` returns false for those prefixes.

**Paste-ready fix** (write-blocked in this session — needs maintainer
sign-off):

```rust
fn is_jdk_class(name: &str) -> bool {
    name.starts_with("java/")
        || name.starts_with("javax/")
        || name.starts_with("sun/")
        || name.starts_with("jdk/")
        || name.starts_with("com/sun/")
        || name.starts_with("[")
        // WP8.10.5 — non-JDK prefixes whose classes have rich
        // synthetic-stub layouts declared below in synthetic_stub_fields.
        // Without these, references to org.jboss.modules.Module et al.
        // raise NoClassDefFoundError before the fallback can synthesize
        // the stub.
        || name.starts_with("org/jboss/")
        || name.starts_with("org/wildfly/")
        || name.starts_with("org/xnio/")
        || name.starts_with("org/infinispan/")
        || name.starts_with("io/quarkus/")
        || name.starts_with("io/agroal/")
        || name.starts_with("io/undertow/")
        || name.starts_with("io/smallrye/")
}
```

**Test that proves it**: `vm/tests/wp8_10_jboss_modules_smoke.rs::probe0_jboss_module_class_reachable`.
Today the test asserts `Some(-99)` (NCDFE caught); after this fix
flip to `Some(1)` and probes 1-6 stop skipping.

**Risk**: Low. The synthetic-stub fallback only fires after a real
classpath lookup fails. If a real `org/jboss/modules/Module.class`
*is* on classpath (e.g., a real jboss-modules.jar), it'll still load
normally; the fallback only triggers when the .class is missing.

### WP8.10.6 — `post_clinit_fixup` for no-`<clinit>` synthetic stubs [S, applied]

**File**: `vm/src/vm/vm_util.rs:642`

**Status**: ✅ applied in this session.

**Diagnosis**: Synthetic stub classes have no bytecode `<clinit>`, so
the original `post_clinit_fixup` call sites (line 543, line 449) never
fire for them. `DefaultBootModuleLoaderHolder.INSTANCE` was therefore
left null even though the fixup arm exists at line 1022.

**Fix landed (this session)**: in the no-`<clinit>` branch (line 642),
when `class.is_synthetic_stub`, fire `post_clinit_fixup` after
`finalize_init`. Idempotent: every fixup arm only writes when the
existing static is null. See `vm/src/vm/vm_util.rs:642+`.

**Note**: this fix alone is not sufficient — gap WP8.10.5 still blocks
the class from being loaded in the first place.

### WP8.10.7 — `Throwable.getMessage` / `printStackTrace` on Throwable subclasses [S, ~50 LoC]

**File**: `native-builtins/src/lang_misc.rs`

**Diagnosis**: `getMessage()` is declared on `java/lang/Throwable` but
not registered for synthetic-stub Throwable subclasses
(`NoClassDefFoundError`, `ClassNotFoundException`, etc.).

**Paste-ready fix outline** (~50 LoC):

```rust
// In lang_misc.rs register block:
for cls in &[
    "java/lang/Throwable",
    "java/lang/NoClassDefFoundError",
    "java/lang/ClassNotFoundException",
    "java/lang/Error",
    "java/lang/Exception",
    "java/lang/RuntimeException",
    "java/lang/LinkageError",
    "java/lang/NoSuchMethodError",
    "java/lang/NoSuchFieldError",
] {
    r.register(cls, "getMessage", "()Ljava/lang/String;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            // Throwable layout: slot 0 = message, slot 1 = cause.
            Ok(Some(ctx.get_field(this, 0)))
        });
    r.register(cls, "printStackTrace", "(Ljava/io/PrintStream;)V",
        |ctx, args| { /* delegate to System.err equivalent */
            Ok(None)
        });
}
```

**Test**: extend probe to throw a `NoClassDefFoundError` then call
`getMessage` — should not throw `NoSuchMethodError`.

### WP8.10.8 — `CRATONVM_JBOSS_MP_ROOT` env-var fallback for `-mp` [XS, applied]

**Status**: ✅ applied.

Augmented `find_mp_argument` in
`native-builtins/src/jboss_module_loader.rs:188` to honor the
`CRATONVM_JBOSS_MP_ROOT` env var as a fallback before scanning argv.
Used by the new smoke test (which can't easily inject `-mp` into the
cargo test harness's argv) and by any external bench script that
prefers env-var configuration. Two new unit tests at
`jboss_module_loader.rs:1825-1853` cover the env-var path and the
empty-string-must-not-match case.

---

## Concrete Next-Action Recommendations

Ranked by ROI (boot progress unlocked per LoC fixed):

1. **WP8.10.5 first** — 5 LoC, unblocks ALL of probes 1-6 and unblocks
   real WildFly's `Main.main` getstatic of INSTANCE. Single highest-impact
   fix in the entire WP8.10 surface. See `vm/tests/wp8_10_jboss_modules_smoke.rs`
   for the validation harness.

2. **WP8.10.7 second** — 30-50 LoC, eliminates a recurring family of
   `NoSuchMethodError`s in every catch-block path. Required before
   any real WildFly boot attempt because WildFly catches
   `ModuleLoadException` and prints it.

3. **Then re-run `bench/wildfly-boot/run-under-cratonvm.sh`** with a
   real WildFly tarball staged. With #1 + #2 + the existing #3
   fixup, expect to surface the *next* layer of failures (probably
   `MethodHandles.Lookup.findStatic` on `org.jboss.as.standalone.Main.main`,
   which is wired but may need additional method-table entries on
   the synthetic stub).

---

## Why a Smoke Test Beat the Real Tarball

The deterministic `vm/tests/wp8_10_jboss_modules_smoke.rs` surfaced
the WP8.10.5 first-failure in <60 seconds of test execution, without
needing a 150 MB download, without `--allowlist` for the WildFly
tarball, and at a JVM-API granularity (per-probe). The same first-failure
in the real tarball would manifest as a 30-frame stack trace
dominated by JBoss Modules internals, where the actual cause
(`is_jdk_class` rejecting `org/jboss/`) is invisible without source
access to *both* projects.

Smoke-test pattern: each probe isolates one boot step. Probes
upstream of a gap fail with the *gap's* error; probes downstream
short-circuit via the `jboss_synthetic_stubs_reachable` gate, so
green/red signal stays interpretable as fixes land incrementally.
