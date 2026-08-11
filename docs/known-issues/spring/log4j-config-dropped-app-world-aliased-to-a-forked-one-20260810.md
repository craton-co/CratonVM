# One library, two worlds: the application loader's log4j is partly the fork's

| | |
|---|---|
| **Status** | **OPEN** (root cause). The surface that made it visible is fixed — reflective argument coercion no longer refuses a same-named copy — but the split itself is still there and now surfaces as a `ClassCastException`. |
| **Scope** | Any suite where a user-defined loader loads a library the application classpath also has. Reproduced on spring-framework `spring-test`, but nothing about it is Spring-specific. |
| **Reproducer** | `TestContextAotGeneratorIntegrationTests` (≈4 min), `CRATONVM_DBG=coerce`. `AotIntegrationTests` shows the same thing at 10× the cost. |
| **Oracle** | HotSpot configures log4j with no errors at all. |

## The symptom

log4j cannot build its `<Logger>` elements:

```
ERROR Could not create plugin of type class …config.LoggerConfig for element Logger:
      java.lang.IllegalArgumentException: argument type mismatch
ERROR Unable to invoke factory method in class …config.LoggerConfig for element Logger:
      java.lang.IllegalStateException: No factory method found for class …LoggerConfig
```

28 times per run, and the configured levels are silently not applied.

## What is actually split

`--dump-class-origins` over one run, `org/apache/logging/log4j/**`:

| | |
|---|---|
| log4j classes loaded | 557 |
| with more than one copy | **554** |
| loader ids in play | `2` (Application) and `3,4,5,6` (four `@CompileWithForkedClassLoader` worlds) |

Two copies of a library across a forked loader is **not** by itself wrong —
`CompileWithForkedClassLoaderClassLoader` extends `testClassLoader.getParent()`,
so `org.apache.logging.log4j.*` misses the parent and the fork defines its own.
HotSpot does the same.

What is wrong is that the copies are not *whole worlds*. Eighteen classes exist
under `3,4,5,6` and **never under `2`**, among them exactly the ones the plugin
machinery needs:

```
config/plugins/convert/TypeConverterRegistry      [3,4,5,6]
config/plugins/convert/TypeConverters             [3,4,5,6]
config/plugins/convert/EnumConverter              [3,4,5,6]
config/plugins/visitors/AbstractPluginVisitor     [3,4,5,6]
config/plugins/visitors/PluginAttributeVisitor    [3,4,5,6]
config/LoggerConfig$LevelAndRefs                  [3,4,5,6]
```

while `Level`, `PluginBuilder`, `LoggerConfig` and `LoggerConfig$Builder` all
have an Application copy. So an Application-world `PluginBuilder` reaches a
forked-world converter and gets back a forked-world `Level`.

`CRATONVM_DBG=coerce` shows the receiver, the declaring class and the field's
declared type are **all** loader 2, and only the value is loader 3 — the
builder side is entirely consistent, the value is the outlier.

## Why the app world is missing those classes

Not because a load was refused. `CRATONVM_DBG=dupclass,dupclass-filter=TypeConverterRegistry`
produces **zero** events over a whole run: `resolve_fast_path_class_id` is never
asked for that name under the Application loader. The app world does not *load*
the forked class — it is *handed* one, already resolved, and every class reached
from there is forked too.

For `Level`, by contrast, the same lever shows the split being created
deliberately:

```
[DBG_DUPCLASS] fallback candidate ClassId(1443) (loader=UserDefined(3), is_user=true)
               for "org/apache/logging/log4j/Level"; delegated_bytes_found=true
[DBG_DUPCLASS] rejecting existing UserDefined-loader candidate … -- delegation chain
               also has it, so a SEPARATE ClassId will be created under Application
```

That rule is right on its own terms. The defect is the *combination*: some names
get a fresh Application copy, others keep pointing at the fork's, and the result
is a world that exists on no real JVM.

## What was fixed, and what this leaves

Reflective argument coercion now fails open when the argument's own superclass
chain carries the expected type's exact binary name (`lang_class.rs`,
`argument_reaches_expected_by_name`) — the same rule the `aastore` predicate has
always applied and that `Array.set` adopted on 2026-08-07. That takes the
refusals from **28 to 0** and the log4j plugin failures from **28 to 4**.

The remaining 4 are the same split reaching a check that must stay strict:

```
ClassCastException: class …lookup.ConfigurationStrSubstitutor
                    cannot be cast to class …lookup.StrSubstitutor
    at …visitors.AbstractPluginVisitor.setStrSubstitutor
```

**Do not "fix" that by making `checkcast` name-lenient.** `jit_checkcast`
refuses a same-named class from a different loader on purpose — its comment
says so in as many words, and that is the whole point of a
`(ClassLoaderId, name)`-keyed dictionary. The CCE is the root cause showing
through honestly.

## Next step

Decide who owns a library when a user-defined loader has already loaded it and
the application classpath also has it — and then make the whole library follow
that decision, rather than resolving it per name. `resolve_fast_path_class_id`
is where the per-name decision is taken today.

The neighbourhood is the same one that produced the compiled-`invokestatic`
defect closed on 2026-08-10 (`fixed-suite-bugs/spring/aotintegration-hangs-after-the-unmodifiable-get-fix.md`):
one binary name, two loaders, and a resolution that answered without asking
which world was asking.
