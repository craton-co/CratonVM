# HIB-CV-27 — in-process javac "compiler message file broken" (FIXED)

## Status
**FIXED + verified** on dev (branch work atop f8cdd52b). Debug binary byte-identical
to HotSpot on the in-process-javac diagnostic path.

## Severity
MEDIUM — broke every H2 feature that compiles Java source in-process
(`CREATE ALIAS ... AS '<java>'`, `CREATE TRIGGER`, user functions), plus any
app that runs `javac`/an annotation processor / JSP-EL compiler at runtime.

## Symptom
```
/Bad.java:1: compiler message file broken: key=compiler.err.error arguments={0}, {1}, ...
compiler message file broken: key=compiler.err.illegal.start.of.expr arguments=...
compiler message file broken: key=compiler.misc.count.error arguments=1, ...
```
`"compiler message file broken: key=…"` is javac's fallback when
`JavacMessages.getLocalizedString` cannot load its message `ResourceBundle`.
H2's `org.h2.util.SourceCompiler` surfaces it as a SQL syntax error.

## Root cause (the earlier hypothesis was WRONG)
The pre-existing bug note guessed "jdk.compiler `.properties` resources are not
served." That is **not** how JDK 25 ships these messages. In the jimage
(`lib/modules`), the javac message tables are **compiled `ListResourceBundle`
subclasses**, NOT `.properties` files:

```
$ jimage list lib/modules | grep javac/resources
    com/sun/tools/javac/resources/compiler.class          <-- ListResourceBundle
    com/sun/tools/javac/resources/compiler_de.class
    com/sun/tools/javac/resources/javac.class
    ...
$ javap com.sun.tools.javac.resources.compiler
    public final class com.sun.tools.javac.resources.compiler
        extends java.util.ListResourceBundle {
      protected final java.lang.Object[][] getContents();
    }
```
There is **no `compiler.properties` at runtime.**

CratonVM intercepts every `ResourceBundle.getBundle(...)` with a Rust native
(`native-builtins/src/locale_resources.rs::rb_get_bundle`) — it bypasses the
JDK's Module/caller-class machinery (which NPEs in our partial bootstrap) and
the LocaleProviderAdapter chain. But that native only knew how to load
**`.properties`** resources (`find_resource("…/compiler.properties")`). For the
javac bundles the `.properties` lookup misses, so the code fell through to an
**empty synthetic bundle**, and javac printed "message file broken" for every
key.

Proof (probe loading the bundle directly, `--nojit`):
* pre-fix: `compiler.err.error = null`
* post-fix: `compiler.err.error = "error: "`

## Fix (`native-builtins/src/locale_resources.rs`)
Teach `rb_get_bundle` the JDK "java.class" bundle format (a compiled
`ListResourceBundle` subclass), tried after the `.properties` lookup fails and
before the synthetic/empty fallback:

1. **`try_class_bundle(ctx, chain)`** — for each candidate in the locale chain
   (ROOT/least-specific first) whose `<name>.class` resource exists, instantiate
   it with the GC-safe `new_object_initialized("<name>", "()V", &[])`, link the
   less-specific ones as the parent chain via `setParent`, and return the
   most-specific bundle. Pins each live bundle across the next instantiation
   (the generated `getContents()` array alloc can trigger a moving GC).
2. Wired into `rb_get_bundle` after the `.properties` chain misses, **skipping
   the curated locale-data families** (`is_synthesized_locale_base`:
   `sun.text.resources.*` / `sun.util.resources.*`) so the existing synthetic
   English/US `FormatData`/`LocaleNames`/… path is untouched.
3. **Parent-chain walk on the read side** — `rb_get_object` already resolved a
   real `ListResourceBundle` via its overridden `getContents()`; on a key miss
   it now walks the bundle's `parent` field before throwing
   `MissingResourceException`, so a locale variant inherits untranslated keys
   from its ROOT parent (e.g. `compiler_de` → `compiler`).

The returned bundle is a **real** `ListResourceBundle`; the read-side natives
(`getString`/`getObject`) resolve keys through its real `getContents()`.

## Verification (debug `cratonvm.exe`, `--nojit`)
End-to-end, instantiating the real `com.sun.tools.javac.api.JavacTool` and
compiling a deliberately-broken source string:

| | diagnostic | result |
|---|---|---|
| pre-fix (cratonvm-devmerge) | `compiler message file broken: key=…` | FAIL |
| **post-fix** | `/Bad.java:1: error: illegal start of expression` | **OK** |
| HotSpot (`--add-exports`) | `/Bad.java:1: error: illegal start of expression` | OK |

CratonVM post-fix output is **byte-identical to HotSpot.**

Regression probe (post-fix == HotSpot): `.properties` app bundle still loads;
`DateFormatSymbols(Locale.US).getMonths()[0] == "January"` (curated FormatData
path intact); absent app bundle still throws `MissingResourceException`.

Probes: `scratch/javacbundle/{JavacToolProbe,JavacBundleProbe,RegressionProbe}.java`.

## Separate, pre-existing follow-up (NOT this bug)
`javax.tools.ToolProvider.getSystemJavaCompiler()` returns **null** on CratonVM
(both pre- and post-fix) — its real-JDK body resolves the system tool through
the boot `ModuleLayer`, which CratonVM does not fully model. Instantiating
`com.sun.tools.javac.api.JavacTool.create()` directly works (that is the exact
path the bundle fix exercises). A synthetic stub exists at
`native-builtins/src/t3_impl.rs` (returns a fake non-compiling `JavaCompiler`)
but is inert in real-JDK mode (not force-listed) — correctly so. Making
`ToolProvider.getSystemJavaCompiler()` return the real `JavacTool` is a distinct
module-layer task; H2's `SourceCompiler` uses `ToolProvider`, so if the H2 test
still fails after this bundle fix it will be on that axis, not the message bundle.
