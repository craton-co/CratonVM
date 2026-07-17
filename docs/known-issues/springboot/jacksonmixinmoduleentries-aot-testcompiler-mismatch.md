# `JacksonMixinModuleEntriesBeanRegistrationAotProcessorTests`/`JsonMixinModuleEntriesBeanRegistrationAotProcessorTests` — AOT-compiled-and-run assertions all fail (3/3)

**Status: OPEN — found 2026-07-17, not root-caused**

**Update 2026-07-17 (bin5 rerun triage) — same signature confirmed in the
sibling `spring-boot-jackson2` module.** `module/spring-boot-jackson2`'s
`org.springframework.boot.jackson2.JsonMixinModuleEntriesBeanRegistrationAotProcessorTests`
(note: `Json...`, not `Jackson...` — a distinct but structurally-identical
sibling test class in Spring Boot's parallel legacy-Jackson/Jackson2
module pair) fails with the **exact same 3 assertion messages** —
`Expecting value to be false but was true` (x2) and `Expecting actual:
given predicate to accept ... RuntimeHints@... but it did not` (x1) — same
test method names, same `tests=3 failed=3`. Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard3/logs/module_spring-boot-jackson2.org.springframework.boot.jackson2.JsonMixinModuleEntriesBeanRegist-6cf2a9850b42.out.log`.

Read the `spring-boot-jackson2` test source this session
(`apps/spring-boot/module/spring-boot-jackson2/src/test/java/org/springframework/boot/jackson2/JsonMixinModuleEntriesBeanRegistrationAotProcessorTests.java`),
which pins down exactly what the two `isFalse()` assertions mean (not
previously spelled out in this doc): `TestConfiguration` (lines 133-150)
declares a `@Bean` factory method `jsonMixinModuleEntries(ApplicationContext)`
that sets `public boolean scanningInvoked = true;` and then does a real
classpath scan (`JsonMixinModuleEntries.scan(...)`). The whole point of
`JsonMixinModuleEntriesBeanRegistrationAotProcessor` is to precompute that
scan result at AOT-processing time and bake it into the generated
bean-registration code, so the **fresh, AOT-generated** context's
`jsonMixinModuleEntries` bean should be constructed from the precomputed
value **without ever calling the original `@Bean` factory method again**
— both failing tests assert `freshContext.getBean(TestConfiguration.class).scanningInvoked`
is `false` for exactly this reason. On CratonVM it's `true`: **the fresh,
AOT-generated context is re-invoking the real `@Bean` factory method (and
therefore re-running the real classpath scan) instead of using the
AOT-precomputed registration.** Since the code doing AOT generation
(`ApplicationContextAotGenerator`) and the code compiling the generated
source (`TestCompiler.forSystem()` → the real `javac`) are both
unmodified Spring/JDK code, this narrows "generation produced different
code" vs. "identical code executes differently under CratonVM" (the two
candidate explanations this doc already named) toward the second: the
generated Java source itself should be identical to what HotSpot would
produce and compile, so the divergence most likely lives in how CratonVM's
`DefaultListableBeanFactory`/`AbstractAutowireCapableBeanFactory`
bean-instantiation path resolves the AOT-generated bean definition at
`GenericApplicationContext.refresh()` time — not confirmed by dumping the
actual generated source this session, but a stronger lead than either
candidate was before. If the same `JsonMixinModuleEntriesBeanRegistrationAotProcessor`-family
code is shared (or near-identical) between the `spring-boot-jackson` and
`spring-boot-jackson2` modules, this single mechanism would explain both
occurrences at once — plausible given the identical failure shape, not
independently verified.

## Symptom

```
Failures (3):
  JacksonMixinModuleEntriesBeanRegistrationAotProcessorTests:processAheadOfTimeWhenPublicClassShouldRegisterClass()
    => org.opentest4j.AssertionFailedError: Expecting value to be false but was true
  JacksonMixinModuleEntriesBeanRegistrationAotProcessorTests:processAheadOfTimeWhenNonAccessibleClassShouldRegisterClassName()
    => org.opentest4j.AssertionFailedError: Expecting value to be false but was true
  JacksonMixinModuleEntriesBeanRegistrationAotProcessorTests:processAheadOfTimeShouldRegisterBindingHintsForMixins()
    => java.lang.AssertionError: Expecting actual: given predicate to accept org.springframework.aot.hint.RuntimeHints@2d43c8 but it did not.
```

Full log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard3/logs/module_spring-boot-jackson.org.springframework.boot.jackson.JacksonMixinModuleEntriesBeanRegis-295cc85e4db2.out.log`

All 3 tests in the class fail (3/3), all via `compile(...)` →
`TestCompiler.compile(...)` (`JacksonMixinModuleEntriesBeanRegistrationAotProcessorTests.java:112,118`)
— i.e. all three exercise Spring's AOT-generation-then-compile-and-run
pipeline (`ApplicationContextAotGenerator` generates Java source for a
`BeanRegistrationAotProcessor`'s contribution, `TestCompiler` compiles and
executes that generated source in a fresh context/classloader), then assert
on the result (a `boolean` "is registered" flag, or a `RuntimeHints`
predicate).

## Root cause

**Not confirmed — no CratonVM source investigation performed this
session; not independently re-run.** This class's whole purpose is
asserting that `JacksonMixinModuleEntriesBeanRegistrationAotProcessor`'s
generated code correctly re-registers Jackson mixin-to-target-class
bindings and reflection hints when replayed from AOT-generated source — all
3 assertions failing (not a partial pass) points at something systemic in
the generate-compile-execute pipeline for this bean, not a narrow edge
case.

This sits in the same general territory as
[`flyway-resourceprovidercustomizer-aot-substitution-not-applied.md`](flyway-resourceprovidercustomizer-aot-substitution-not-applied.md)
(a `spring-boot-flyway` class where `TestCompiler`-executed AOT-generated
substitution code silently doesn't apply) — both are
`BeanRegistrationAotProcessor` contributions verified via
`TestCompiler.compile(...)`, and both fail with the AOT-generated behavior
not taking effect (there: wrong bean instance returned; here: hint/registration
predicates false when expected true). **Not confirmed to share the exact
same mechanism** — the Flyway doc's own two candidate explanations
(generation-side vs. compile/execution-side) apply equally here and are
equally untested for this class; filing separately rather than merging
since neither has been root-caused enough to say they're the same bug.

Also adjacent to the already-**FIXED**
[`../../internal/springboot/testcompiler-annotation-classes-not-found-cluster-FIXED.md`](../../internal/springboot/testcompiler-annotation-classes-not-found-cluster-FIXED.md)
(a different, already-resolved `TestCompiler` gap: JRT package listing /
generated in-memory `resource:` URL handling) — this failure shape is
different (assertions run to completion and produce a wrong boolean/hints
result, not a `ClassNotFoundException` during compilation), so it is not
simply a residual of that fix, but confirms `TestCompiler`-driven AOT tests
remain a recurring source of CratonVM-specific divergence worth a dedicated
look.

**Next step for whoever picks this up:** dump the AOT-generated Java source
for this specific processor (`generationContext.writeGeneratedContent()`)
from a standalone repro and diff against real HotSpot's generated source —
same diagnostic recommended in the Flyway doc — to distinguish
"generation produced different code" from "identical code executes
differently under CratonVM."

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-jackson2` | `org.springframework.boot.jackson2.JsonMixinModuleEntriesBeanRegistrationAotProcessorTests` (all 3 test methods, added bin5) |
| `module/spring-boot-jackson` | `org.springframework.boot.jackson.JacksonMixinModuleEntriesBeanRegistrationAotProcessorTests` (all 3 test methods) |
