# One library, two worlds: the application loader's log4j is partly the fork's

| | |
|---|---|
| **Status** | **CLOSED 2026-08-10**, retired here out of `known-issues/spring/`. log4j now configures with **zero** errors on both reproducers, matching HotSpot. The split was created by annotation-element resolution: a `Class`-valued member fell through to a loader-blind lookup, so an application-loaded holder was handed a forked loader's copy. Read "What it actually was" first — the "Next step" this page proposed while open named the wrong function. |
| **Scope** | Any suite where a user-defined loader loads a library the application classpath also has. Reproduced on spring-framework `spring-test`, but nothing about it is Spring-specific. |
| **Reproducer** | `TestContextAotGeneratorIntegrationTests` (≈4 min), `CRATONVM_DBG=coerce`. `AotIntegrationTests` shows the same thing at 10× the cost. In-tree: `vm/tests/annotation_loader_isolation.rs` STEP 6. |
| **Oracle** | HotSpot configures log4j with no errors at all. |

## The symptom

log4j could not build its `<Logger>` elements:

```
ERROR Could not create plugin of type class …config.LoggerConfig for element Logger:
      java.lang.IllegalArgumentException: argument type mismatch
ERROR Unable to invoke factory method in class …config.LoggerConfig for element Logger:
      java.lang.IllegalStateException: No factory method found for class …LoggerConfig
```

28 times per run, and the configured levels silently not applied.

## What is actually split

`--dump-class-origins` over one run, `org/apache/logging/log4j/**`:

| | before | after |
|---|---|---|
| log4j classes loaded | 557 | 548 |
| with more than one copy | 554 | 547 |
| **existing only under a fork** | **18** | **2** |
| loader ids in play | `2` (Application) and `3,4,5,6` (four `@CompileWithForkedClassLoader` worlds) | same |

Two copies of a library across a forked loader is **not** by itself wrong —
`CompileWithForkedClassLoaderClassLoader` extends `testClassLoader.getParent()`,
so `org.apache.logging.log4j.*` misses the parent and the fork defines its own.
HotSpot does the same.

What was wrong is that the copies were not *whole worlds*. Eighteen classes
existed under `3,4,5,6` and **never under `2`**, among them exactly the ones the
plugin machinery needs:

```
config/plugins/convert/TypeConverterRegistry      [3,4,5,6]
config/plugins/convert/TypeConverters             [3,4,5,6]
config/plugins/convert/EnumConverter              [3,4,5,6]
config/plugins/visitors/AbstractPluginVisitor     [3,4,5,6]
config/plugins/visitors/PluginAttributeVisitor    [3,4,5,6]
config/LoggerConfig$LevelAndRefs                  [3,4,5,6]
```

while `Level`, `PluginBuilder`, `LoggerConfig` and `LoggerConfig$Builder` all
had an Application copy. So an Application-world `PluginBuilder` reached a
forked-world converter and got back a forked-world `Level`.

`CRATONVM_DBG=coerce` showed the receiver, the declaring class and the field's
declared type were **all** loader 2, and only the value was loader 3 — the
builder side entirely consistent, the value the outlier.

## Why the app world was missing those classes

Not because a load was refused. `CRATONVM_DBG=dupclass,dupclass-filter=TypeConverterRegistry`
produced **zero** events over a whole run: `resolve_fast_path_class_id` was never
asked for that name under the Application loader. The app world did not *load*
the forked class — it was *handed* one, already resolved, and every class
reached from there was forked too.

That "handed one" is the whole bug, and it is one code path.

## What it actually was

`PluginBuilder.injectFields` asks each field annotation for its visitor:

```java
PluginVisitorStrategy strategy = annoClass.getAnnotation(PluginVisitorStrategy.class);
return strategy.value().newInstance();      // a Class-valued annotation member
```

`strategy.value()` is an `AnnotationElementValue::Class`, resolved in
`native-builtins/src/lang_class.rs`, `annotation_element_to_java_typed`. That arm
tried, in order:

1. the container's `ClassLoader` object, when the container has one —
   HotSpot's `AnnotationParser.parseClassValue(sig, container)`;
2. `class_id_by_name_near(name, container)` — the container's own namespace plus
   the built-in delegation chain it inherits;
3. **`class_id_by_name(name)` — the loader-blind global lookup.**

An Application-loaded container has no `ClassLoader` object, so step 1 was
skipped by construction ("`None` for built-in loaders keeps the global
resolution", `annotation_container_loader`). Step 2 answers only for names that
loader has *already* resolved, and the app world had never touched
`PluginAttributeVisitor`. So step 3 answered — and step 3 returns whichever
single loader happens to have the name, which here was the fork.

`CRATONVM_IAE_TRACE=1`, one run, before the fix:

```
ANN-CLASS class=…visitors/PluginElementVisitor          holder=2286/L2 via=GLOBAL answer=L3
ANN-CLASS class=…visitors/PluginConfigurationVisitor    holder=2290/L2 via=GLOBAL answer=L3
ANN-CLASS class=…visitors/PluginBuilderAttributeVisitor holder=2263/L2 via=GLOBAL answer=L3
ANN-CLASS class=…visitors/PluginAttributeVisitor        holder=2435/L2 via=GLOBAL answer=L3
ANN-CLASS class=…validation/validators/RequiredValidator holder=2411/L2 via=GLOBAL answer=L3
```

Five members. Holder loader 2, answer loader 3, every time. Everything the
visitors then reach — `AbstractPluginVisitor`, `TypeConverters`,
`TypeConverterRegistry`, `EnumConverter` — is resolved from inside the fork's
world and never appears under `2` at all. That is the whole 18-class list,
minus the handful the fork's own code pulls in for itself.

### The fix

Drive the container's own loader before conceding to the loader-blind lookup:

```rust
let driven = scoped.is_none().then(|| {
    container_class_id.and_then(|holder| {
        ctx.class_id_by_name_via_referencing_class(holder, class_name).ok()
    })
}).flatten();
if let Some(cid) = scoped.or(driven).or_else(|| ctx.class_id_by_name(class_name)) { … }
```

`class_id_by_name_via_referencing_class` resolves `name` as a bytecode reference
*from* `holder` would (JVMS §5.4.3 initiating loader) — for an Application
holder that is the ordinary delegation chain, which finds log4j-core on the
classpath and defines an Application copy. It does not run `<clinit>`, so it
matches HotSpot's `Class.forName(name, false, container.getClassLoader())`.

The loader-blind lookup stays as the last resort: purely in-memory classes
(`Proxy`-generated annotation proxies and the like) have no classpath bytes, so
nothing else can answer for them.

**This step already existed** — the sibling `Enum` arm has taken it since the
`SpringBootContextLoaderAotTests` fix, and its comment describes this exact
failure shape for enum constants. The `Class` arm was simply never converted.
The array-component arm had the same gap and is converted here too.

### Result

| | log4j `ERROR` | `ClassCastException` | fork-only log4j classes |
|---|---|---|---|
| before the coercion fix | 28 | 0 | 18 |
| after the coercion fix (this page's "OPEN" state) | 4 | 6 | 18 |
| **after this fix** | **0** | **0** | **2** |

`TestContextAotGeneratorIntegrationTests` `found=4 succ=4 fail=0`,
`AotIntegrationTests` `found=4 succ=2 fail=0 skip=2` — both with zero log4j
output, which is the HotSpot oracle.

The two names still fork-only, `LoggerConfig$LoggerConfigPredicate` and
`message/Message`, are not residuals of this defect: nothing in the application
world ever resolves them, so there is nothing to define an Application copy for.
A class the app world never references is absent there on HotSpot too. What
changed is that any app-world resolution of a log4j name now *stays* in the app
world — `PluginAttributeVisitor` has a loader-2 copy in the post-fix dump.

## What the earlier reflective-coercion fix did

Reflective argument coercion fails open when the argument's own superclass chain
carries the expected type's exact binary name (`lang_class.rs`,
`argument_reaches_expected_by_name`) — the same rule the `aastore` predicate has
always applied and that `Array.set` adopted on 2026-08-07. That took the
refusals from 28 to 0 and the log4j plugin failures from 28 to 4. It is still
correct and still in place; it was masking, not causing.

The 4 that survived it were the same split reaching a check that must stay
strict:

```
ClassCastException: class …lookup.ConfigurationStrSubstitutor
                    cannot be cast to class …lookup.StrSubstitutor
    at …visitors.AbstractPluginVisitor.setStrSubstitutor
```

**Do not "fix" that by making `checkcast` name-lenient.** `jit_checkcast`
refuses a same-named class from a different loader on purpose — its comment
says so in as many words, and that is the whole point of a
`(ClassLoaderId, name)`-keyed dictionary. The CCE was the root cause showing
through honestly, and it went away when the root cause did.

## What this page got wrong while it was open

> Decide who owns a library when a user-defined loader has already loaded it and
> the application classpath also has it — and then make the whole library follow
> that decision, rather than resolving it per name. `resolve_fast_path_class_id`
> is where the per-name decision is taken today.

The conclusion was right and the location was wrong. Ownership does not need a
new policy: the JVMS rule (resolve through the referencing class's initiating
loader) already decides it, and following that rule *per name* is precisely what
makes a whole world — every app-world resolution lands in the app world, so the
world closes over itself. `resolve_fast_path_class_id` was never reached for the
handed-over names, which the page's own zero-dupclass-events finding says
plainly; the caller that bypassed it was the one to fix.

## Regression guard

`vm/tests/annotation_loader_isolation.rs`, STEP 6 of
`vm/tests/resources/annprobe/AnnProbe.java`: STEP 5 leaves `AnnProbeRefType`
defined only by the filtering child loader, then an **application**-loaded
holder reads the same `Class`-valued member. It must answer with the application
copy. Verified RED on the pre-fix binary
(`value()=AnnProbeRefType loadedBy=AnnProbe$FilteringClassLoader`) and GREEN
after, with HotSpot passing both.

## Neighbourhood

Same one that produced the compiled-`invokestatic` defect closed on 2026-08-10
(fixed-suite-bugs/spring/aotintegration-hangs-after-the-unmodifiable-get-fix.md):
one binary name, two loaders, and a resolution that answered without asking
which world was asking. Three arms of the same annotation-element conversion
have now needed the same conversion — `Enum`, `Class`, and the array component.
