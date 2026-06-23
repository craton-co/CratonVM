# HIB-CV-26 — `Class.forName(name, true, loader)` (3-arg, non-null loader) skips `<clinit>` → JDBC driver self-registration never fires → `DriverManager.getDriver` throws "No suitable driver"

**Severity:** Medium — at least 1 CV-only failing class (`DriverManagerRegistrationTest`); latent for any code that relies on the 3-arg `Class.forName` overload's *initialize=true* contract (driver auto-registration, SPI/DI eager init, `@BeforeClass` static-init side effects).
**Status:** ✅ FIXED — `native-builtins/src/lang_class.rs` (`native_class_for_name`, loader-success branch).
**Mode:** Interpreter (`--nojit`); deterministic. Not a JIT bug.
**HotSpot:** not affected.

## Symptom

`org.hibernate.orm.test.connection.DriverManagerRegistrationTest.testDriverRegistrationUsingClassForNameSucceeds`
fails (HotSpot passes) with:

```
org.opentest4j.AssertionFailedError: Unanticipated failure according to HHH-7272
  at ...DriverManagerRegistrationTest.testDriverRegistrationUsingClassForNameSucceeds(DriverManagerRegistrationTest.java:68)
```

The test does:

```java
Class.forName("…$TestDriver2", true, determineClassLoader());   // <-- 3-arg, non-null loader
assertNotNull( DriverManager.getDriver("jdbc:hibernate:test2") );  // throws SQLException "No suitable driver"
```

`TestDriver2` registers itself with `DriverManager` from its `static {}` block. On CratonVM that
static initializer never ran, so the driver was never in `registeredDrivers`, so `getDriver` found
no match and threw `SQLException("No suitable driver")`, tripping the `fail()` in the `catch`.

## Root cause

`DriverManager` runs as **real JDK bytecode** here (trace shows `java/sql/DriverManager.getDriver`,
`java/sql/DriverManager.ensureDriversInitialized`). The bug is upstream, in `Class.forName`.

CratonVM's `native_class_for_name` (the `forName0` / public-`forName` native) has a dedicated branch
for when a **non-null classloader** is supplied (args[2]): it routes through
`loader.loadClass(name)` so module-scoped loaders get their visibility search
(`native-builtins/src/lang_class.rs:1444`). On success it returned the resolved mirror **directly**:

```rust
Ok(Some(mirror)) => {
    s111_dbg!("[S111-DBG] loadClass({}) succeeded via invoke_virtual", dotted_name);
    return Ok(Some(mirror));     // <-- BUG: never initialised, ignores args[1]
}
```

`ClassLoader.loadClass` only **loads + links** a class; per the JLS it does **not** run static
initialisers. The `initialize` flag (`args[1]`) was dropped on the floor for this branch, so
`Class.forName(name, true, loader)` returned an *uninitialised* class. The bootstrap branch lower in
the same function (`ctx.ensure_class_initialized(&internal_name)`,
`native-builtins/src/lang_class.rs:1582`) *does* initialise — which is why the **1-arg**
`Class.forName(name)` (null loader → bootstrap branch) always worked and masked the gap.

Minimal repro (`MinForName.java`), CratonVM vs HotSpot:

| call | HotSpot runs clinit | CratonVM (before fix) |
|------|---------------------|-----------------------|
| `Class.forName(n, true, cl)`  | yes | **no** ← bug |
| `Class.forName(n)` (1-arg)    | yes | yes |
| `Class.forName(n, false, cl)` | no  | no |

Why `DriverManagerRegistrationTest`'s *other* (`…UsingLoadClassFails`) test still passed: it calls
`loader.loadClass()` directly and **expects** no registration — so the missing init coincidentally
matched the expectation. Only the `…ClassForNameSucceeds` case exposed the divergence.

## Fix

On the loader-success branch, honour `args[1]`: if `initialize` is true, initialise the resolved
class before returning. The class id is recovered from the returned mirror
(`class_id_from_mirror`) and initialised by its actual binary name
(`ensure_class_initialized`), so any `ExceptionInInitializerError` / linkage error raised by
`<clinit>` propagates exactly as HotSpot's `Class.forName` does.

```rust
let initialize = matches!(args.get(1), Some(v) if v.as_int().unwrap_or(0) != 0);
if initialize {
    if let Value::Object(Some(mirror_ref)) = mirror {
        if let Some(cid) = ctx.class_id_from_mirror(mirror_ref) {
            if let Some(bin_name) = ctx.class_name_of_id(cid) {
                ctx.ensure_class_initialized(&bin_name)?;
            }
        }
    }
}
return Ok(Some(mirror));
```

`initialize=false` is unaffected (still load-only — correct), and the bootstrap (null-loader) branch
is unchanged.

## Verification

- `MinForName` probe: 3-arg `forName(n,true,cl)` now runs `<clinit>` (matches HotSpot); `false` still
  doesn't; 1-arg still does.
- `DrvProbe2` probe (self-registering driver loaded via 3-arg `forName`, then `DriverManager.getDriver`):
  before = `selfDriverPresent=false` / `getDriver THREW: No suitable driver`; after = registered +
  `getDriver SUCCESS`.
- Full class `DriverManagerRegistrationTest` under the suite runner: PASS (matches HotSpot).

## Repro

Build release `cratonvm` from dev; from `apps/hibernate-orm/.cratonvm-suite`:

```
cratonvm --nojit @common.args -Dcraton.trace=1 CratonRunner <listfile> 0
```
where `<listfile>` contains `org.hibernate.orm.test.connection.DriverManagerRegistrationTest`.
