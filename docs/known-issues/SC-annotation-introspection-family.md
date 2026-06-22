## Symptom
Twelve tests across the Spring annotation-introspection family fail under CratonVM (all pass on HotSpot/JDK25). They split into FOUR independent CratonVM divergences.

## Affected tests
- `AnnotationIntrospectionFailureTests` (all 4): `filteredTypeThrowsTypeNotPresentException`, `filteredTypeInMetaAnnotationWhenUsingAnnotatedElementUtilsHandlesException`, `filteredTypeInMetaAnnotationWhenUsingMergedAnnotationsHandlesException`, `filteredTypeInAnnotationAttributeDoesNotThrowWhenCallingAsAnnotationAttributes`
- `AnnotationFilterTests.matchesAnnotationWhenMatchReturnsTrue`, `AnnotationFilterTests.matchesAnnotationClassWhenMatchReturnsTrue`
- `AnnotationTypeMappingsTests.forAnnotationTypeWhenRepeatableMetaAnnotationIsFiltered`
- `MergedAnnotationsRepeatableAnnotationTests.typeHierarchyAnnotationsWithLocalComposedAnnotationWhoseRepeatableMetaAnnotationsAreFiltered`
- `MergedAnnotationClassLoaderTests.synthesizedUsesCorrectClassLoader`
- `AnnotationsScannerTests.typeHierarchyStrategyWithEnclosingClassPredicatesOnEnclosedInnerClassScansAnnotations`, `...OnEnclosedStaticClassScansAnnotations`
- `MergedAnnotationsComposedOnSingleAnnotatedElementTests.typeHierarchyStrategyMultipleComposedAnnotationsOnBridgeMethod`

## Root cause вЂ” this cluster has FOUR distinct root causes

### (A) Class-valued annotation attribute: no TypeNotPresentException, no classloader context  [HIGH]
Class attributes are converted **eagerly** at proxy build time, not lazily on accessor invocation. `create_annotation_proxy` calls `annotation_element_to_java_typed` for every element up front (`native-builtins/src/lang_class.rs:7893-7898`). For a `Class<?>` value the `AnnotationElementValue::Class(desc)` arm (`lang_class.rs:8050-8094`):
- resolves the class with `ctx.load_class(class_name)` (`:8064`), but `NativeContext::load_class(&mut self, name: &str)` (`native-api/src/registry.rs:213`) takes **no ClassLoader** вЂ” so it resolves the type name through the global/app loader, ignoring the annotation type's defining loader (the test's child `FilteringClassLoader`/`OverridingClassLoader`); and
- on failure it `return Value::Object(None)` (`:8078`) вЂ” i.e. returns `null` instead of throwing `TypeNotPresentException` (cause `ClassNotFoundException`).

`TypeNotPresentException` exists only as a registered constructor (`native-builtins/src/lib.rs:38322`) and is never thrown anywhere in the VM. Consequences:
- `filteredTypeThrowsTypeNotPresentException` expects `value()` to throw `TypeNotPresentException`/cause `ClassNotFoundException`; CratonVM returns null (or the globally-resolved real class) в†’ fail.
- `filteredTypeInAnnotationAttributeDoesNotThrowWhenCallingAsAnnotationAttributes` expects `getClass("value")` to throw `TypeNotPresentException` and `asAnnotationAttributes` to *store the exception as the attribute value* в†’ fail.
- The two meta-annotation tests expect graceful null/false because the underlying `Class` type cannot be loaded *by the child loader*; CratonVM either silently resolves it globally (so it is "present") or returns null at the wrong layer в†’ fail.
- `MergedAnnotationClassLoaderTests.synthesizedUsesCorrectClassLoader` asserts `getClassAttribute(metaAnnotation).getClassLoader() == child`; because the `Class` attribute (`@TestMetaAnnotation(classValue = TestReference.class)`) is resolved without the defining-loader context, the returned mirror's loader is not the child в†’ fail. (Note: plain `Class.getClassLoader()` for child-defined classes *is* correct via `defining_loader_for`, `lang_class.rs:10906`; the gap is specifically the annotation `Class`-attribute resolution path.)

Correct fix shape: thread the annotation type's defining ClassLoader into the `Class`-attribute resolution, resolve **lazily** in `annotation_proxy_invoke`, and on `ClassNotFoundException` raise `TypeNotPresentException` (cause CNFE) rather than returning null.

### (B) Lambda SAM dispatch swallows same-name/same-arity interface default methods  [HIGH]
`AnnotationFilter` is a `@FunctionalInterface` whose abstract SAM is `boolean matches(String)`, with default methods `boolean matches(Class<?>)` and `boolean matches(Annotation)` that first convert to a type name. All three are named `matches` and the two defaults have **arity 1**, same as the SAM.

CratonVM's lambda dispatch intercepts a call iff `method_name == sam_method_name && arg_count == sam_arity`, with **no parameter-type/descriptor check**:
- `vm/src/vm/vm_exec.rs:4828-4832`
- `vm/src/runtime/interpreter.rs:13639-13643`

So `FILTER.matches(someClass)` and `FILTER.matches(someAnnotation)` are routed straight into the lambda body implementing `matches(String)`, passing a `Class`/`Annotation` object where a `String` is expected. The lambda then evaluates `nullSafeEquals(<object>, "...TestAnnotation")` в†’ always `false`. This exactly explains the pass/fail split: the `*WhenMatchReturnsTrue` tests get a wrong `false` (FAIL) while the `*WhenNoMatchReturnsFalse` tests get an accidental `false` (PASS). Same mechanism breaks the `AnnotationFilter`-based filters in `AnnotationTypeMappingsTests.forAnnotationTypeWhenRepeatableMetaAnnotationIsFiltered` (`Repeating.class.getName()::equals`) and `MergedAnnotationsRepeatableAnnotationTests.typeHierarchyAnnotationsWithLocalComposedAnnotationWhoseRepeatableMetaAnnotationsAreFiltered` (`PeteRepeat.class.getName()::equals`) вЂ” those method-reference filters also implement `matches(String)` but are invoked through `matches(Class)`/`matches(Annotation)`.

Correct fix shape: only intercept when the invoked descriptor matches the SAM descriptor (or the parameter types are SAM-compatible); when `method_name == sam_name && arity matches` but the descriptor differs (an overloaded same-arity default), fall through (`return Ok(None)` / take the non-lambda branch) so the interface default method runs and then re-invokes the SAM. The existing arity guard was added for differing-arity defaults (JUnit5 `TestInstancesProvider`); it must be extended to the equal-arity / differing-parameter-type case.

### (C) Enclosing-class TYPE_HIERARCHY scan divergence  [MEDIUM-LOW]
`typeHierarchyStrategyWithEnclosingClassPredicatesOnEnclosed{Static,Inner}ClassScansAnnotations` walk `AnnotationsScanner.processClassHierarchy`'s enclosing branch (Spring `AnnotationsScanner.java:208-218`) driven by predicates `ClassUtils::isStaticClass` (= `Modifier.isStatic(getModifiers())`) and `ClassUtils::isInnerClass` (= `isMemberClass() && !isStatic`), comparing the predicate-driven result list against `Search.always`, expecting `["0:EnclosedThree","1:EnclosedTwo","2:EnclosedOne"]`. The supporting natives all exist: `getModifiers()` reads the `InnerClasses` access flags (incl. ACC_STATIC) at `lang_class.rs:7548-7557`, `getDeclaringClass0` at `:10992`, `getEnclosingMethod0` at `:11064`, and `InnerClasses` is parsed (`classloading/src/class_manager.rs:2812`). So this is not a missing-feature failure but a subtler edge case вЂ” most likely the `InnerClasses` flag for the *non-static inner* nested types (so `isStaticClass`/`isInnerClass` misclassify one enclosing level) or an enclosing-traversal ordering mismatch between the predicate path and `Search.always`. Needs a targeted repro of `getModifiers()`/`getEnclosingClass()` on `AnnotationEnclosingClassSample.EnclosedInner.EnclosedInnerInner` to localize. (This is NOT the Bug-B lambda issue: `Predicate.test` has no same-name overloaded default.)

### (D) Bridge-method annotation merge  [MEDIUM-LOW]
`typeHierarchyStrategyMultipleComposedAnnotationsOnBridgeMethod` selects javac's synthetic bridge `getFor(Class):Object` on `StringGenericParameter` (overrides `GenericParameter<String>.getFor`), asserts `isBridge()`, then `MergedAnnotations.from(bridge, TYPE_HIERARCHY)` must surface `@FooCache`/`@BarCache` declared on the *real* `getFor(Class<String>):String`. `isBridge()` works (ACC_BRIDGE 0x0040 via `modifier_check` at `lang_class.rs:9826`). The bridge itself carries no annotations, so Spring relies on `BridgeMethodResolver.findBridgedMethod()` (which matches candidates by name + erased/generic parameter types) to find the annotated method. The likely divergence is the bridgeв†’bridged resolution failing under CratonVM's generic parameter-type reflection (a known historically-incomplete area), so the resolver returns the bridge (no annotations) and the stream is empty. Needs confirmation that CratonVM exposes BOTH `getFor` overloads with the bridge flagged and that `getGenericParameterTypes()` on the real method resolves `Class<String>`.

## Reproduction sketch
See the repro_sketch field for a minimal standalone Bug-B reproducer (`FilterRepro.java`) and the Bug-A approach. For the full cluster, run each failing class via the JUnit ConsoleLauncher, e.g.:
`cratonvm-spring0621-dev.exe --java-home <jdk25> -cp <spring-core-test-cp> org.junit.platform.console.ConsoleLauncher -c org.springframework.core.annotation.AnnotationFilterTests`
(Do NOT run while the full suite is in progress.)

## Suspected subsystem
native-builtins annotation proxy (`lang_class.rs` Class-attribute resolution) + classloader-context plumbing (`native-api/registry.rs` `load_class`) for Bug A; vm lambda SAM dispatch (`vm_exec.rs` / `interpreter.rs`) for Bug B; AnnotationsScanner enclosing-class + reflection (`lang_class.rs` getModifiers/InnerClasses) for Bug C; bridge/generic-type reflection for Bug D.

## Severity / Confidence
Severity high (annotation introspection underpins much of Spring/JUnit; the lambda default-method mis-dispatch in particular is a general correctness bug affecting any functional interface with same-name/same-arity default overloads). Confidence high for A and B (exact cite + mechanism), medium-low for C and D (infrastructure present; needs a targeted repro to pin the precise divergence).

## Recommendation
Fix. Bug B is a small, high-value, contained change at two cite points (add a descriptor/param-type check to the SAM interception so equal-arity overloaded defaults fall through). The throw-side of Bug A (raise TypeNotPresentException instead of returning null) is also contained; the classloader-context part of Bug A (thread the defining loader through `load_class`/resolution and make Class-attr resolution lazy) is a slightly larger but still well-scoped follow-up. Bugs C and D are best handled as separate focused tasks after a repro confirms the exact divergence.

## Open questions
1. Bug A: what is the cleanest way to thread the annotation type's defining ClassLoader into the Class-attribute resolution (extend `load_class` with a loader arg, or resolve via the annotation type's ClassId's defining loader at accessor time)?
2. Bug A: should Class-attribute resolution become lazy in `annotation_proxy_invoke` so the exception surfaces on accessor invocation (HotSpot semantics) and `asAnnotationAttributes` can store the exception object as the value?
3. Bug B: are there other equal-arity same-name default overloads elsewhere that currently rely on the buggy interception (regression risk when tightening the match)?
4. Bug C: does `getModifiers()` return the correct ACC_STATIC bit (and `isMemberClass()` the correct value) for `AnnotationEnclosingClassSample.EnclosedInner.EnclosedInnerInner`, and does `getEnclosingClass()` chain through all three levels?
5. Bug D: does CratonVM expose the bridge `getFor(Class):Object` with ACC_BRIDGE|ACC_SYNTHETIC, and does `getGenericParameterTypes()` on the real `getFor(Class<String>)` resolve so `BridgeMethodResolver` can match it?

---
## UPDATE (2026-06-21, post-dev-merge re-investigation)

**Bug B — FIXED & on dev** (lambda SAM param-type dispatch, commit 00c71bb6 → merged): AnnotationFilterTests 11/11, AnnotationTypeMappingsTests 43/43, MergedAnnotationsRepeatableAnnotationTests 24/24.

**Bug A — BLOCKED (root cause was wrong):** real blocker is that CratonVM ignores user `ClassLoader`s for `forName` — see `SC-custom-classloader-ignored.md`. The defining-loader annotation fix is correct-in-shape but inert (annotation type loads under AppClassLoader, not the FilteringClassLoader). Needs the foundational class-loading fix first.

**Bug C — root cause was wrong:** `getModifiers`/`isMemberClass`/`getEnclosingClass` are all CORRECT on CratonVM (probe matches HotSpot). The 2 AnnotationsScannerTests failures are in Spring's scan *traversal*, not reflection.

**Bug D — root cause was wrong; NOT cleanly fixable:** bridge resolution + annotation composition are CORRECT (`MergedAnnotations.from(bridge,TYPE_HIERARCHY).stream(Cacheable)` yields exactly `[fooKey,barKey]`/`[fooCache,barCache]`, == HotSpot). The failure is `getDeclaredMethods()` ordering: the test's `getBridgeMethod()` (`methods.get(0).getReturnType()==Object ? get(0) : get(1)`) assumes HotSpot's order `[bridge, getFor(Integer), getFor(Class<String>)]`; CratonVM returns class-file/declaration order `[getFor(Class<String>), getFor(Integer), bridge]`, so it picks `getFor(Integer)` (not a bridge) → `isBridge()` assert fails. HotSpot's order is JVMS-unspecified (HotSpot symbol-table `Method::sort_methods`), not replicable without emulating it; changing `getDeclaredMethods` order globally is broad-risk. **No clean fix.**
