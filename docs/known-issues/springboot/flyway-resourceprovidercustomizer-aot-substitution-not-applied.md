# `ResourceProviderCustomizerBeanRegistrationAotProcessorTests` — AOT-generated bean substitution never takes effect

**Status: OPEN — found 2026-07-17, not root-caused**

## Symptom

Module `module/spring-boot-flyway`, class
`ResourceProviderCustomizerBeanRegistrationAotProcessorTests`, test
`shouldReplaceResourceProviderCustomizer()`:

```
=> java.lang.AssertionError:
Expecting actual:
  org.springframework.boot.flyway.autoconfigure.ResourceProviderCustomizer@27af21
to be an instance of:
  org.springframework.boot.flyway.autoconfigure.NativeImageResourceProviderCustomizer
but was instance of:
  org.springframework.boot.flyway.autoconfigure.ResourceProviderCustomizer
       org.springframework.boot.flyway.autoconfigure.ResourceProviderCustomizerBeanRegistrationAotProcessorTests.lambda$shouldReplaceResourceProviderCustomizer$0(ResourceProviderCustomizerBeanRegistrationAotProcessorTests.java:75)
       org.springframework.boot.flyway.autoconfigure.ResourceProviderCustomizerBeanRegistrationAotProcessorTests.lambda$compile$0(ResourceProviderCustomizerBeanRegistrationAotProcessorTests.java:98)
       org.springframework.core.test.tools.TestCompiler.compile(TestCompiler.java:289)
```

The other 2 tests in this class pass. Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard3/logs/module_spring-boot-flyway.org.springframework.boot.flyway.autoconfigure.ResourceProviderCustom-c52356529503.out.log`

## What the test actually does (confirmed from source)

`apps/spring-boot/module/spring-boot-flyway/src/test/java/.../ResourceProviderCustomizerBeanRegistrationAotProcessorTests.java`:

```java
void shouldReplaceResourceProviderCustomizer() {
    compile(createContext(ResourceProviderCustomizerConfiguration.class), (freshContext) -> {
        freshContext.refresh();
        ResourceProviderCustomizer bean = freshContext.getBean(ResourceProviderCustomizer.class);
        assertThat(bean).isInstanceOf(NativeImageResourceProviderCustomizer.class);
    });
}
```

`compile(...)` runs Spring's `ApplicationContextAotGenerator.processAheadOfTime`
against the original context, writes the generated Java source, then
compiles and executes it via `TestCompiler.forSystem()...compile(...)` — a
**fresh, second application context** built entirely from the AOT-generated
source. The `BeanRegistrationAotProcessor` for `ResourceProviderCustomizer`
is supposed to have contributed generated code that swaps in
`NativeImageResourceProviderCustomizer` in place of the plain
`ResourceProviderCustomizer` during that AOT generation pass — that's the
entire point of the test. Instead, the bean actually obtained from the
AOT-compiled-and-run context is the **original, unmodified**
`ResourceProviderCustomizer`, meaning the AOT bean-registration
substitution silently never took effect end-to-end (either it wasn't
generated into the source, or the generated substitution code didn't run
when the compiled context refreshed).

## Root cause

**Not confirmed.** Two candidate failure points, neither investigated at
the CratonVM source level this session:

1. **Generation-side**: the `BeanRegistrationAotProcessor` contribution for
   `ResourceProviderCustomizer` itself is real Spring bytecode/logic — if
   CratonVM's AOT processing pass (`ApplicationContextAotGenerator`) doesn't
   correctly drive processor contributions (e.g. reflection over
   `BeanRegistrationAotProcessor` implementations found via
   `BeanFactoryUtils`/`SpringFactoriesLoader` doesn't discover this one),
   the generated source would simply never contain the substitution in the
   first place.
2. **Compile/execution-side**: `TestCompiler.forSystem()...compile(...)`
   compiles the generated source in-memory and loads/executes it via a
   fresh `URLClassLoader`/in-memory compiler — this exact mechanism
   (`TestCompiler`) was the subject of a previously-**FIXED** cluster,
   [`../../internal/springboot/testcompiler-annotation-classes-not-found-cluster-FIXED.md`](../../internal/springboot/testcompiler-annotation-classes-not-found-cluster-FIXED.md)
   ("full JRT package listing and generated in-memory `resource:` URL
   handling fixed, 94/94 tests pass"). This failure is **not** the same
   symptom as that cluster (no `ClassNotFoundException`/annotation-listing
   error here — the compiled code runs and produces a context, just with
   the wrong bean), so it is not simply a residual of that fix, but it sits
   in the same general "AOT-generated code compiled+run via `TestCompiler`
   behaves subtly differently than real HotSpot" territory and is worth
   checking against once that area is revisited.

**Next step for whoever picks this up:** dump the actual AOT-generated
Java source (`generationContext.writeGeneratedContent()`'s output directory)
from a standalone repro and diff it against what real HotSpot generates for
the same input — that would immediately distinguish "generation produced
the wrong source" from "generation was correct but execution didn't apply
it."

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-flyway` | `org.springframework.boot.flyway.autoconfigure.ResourceProviderCustomizerBeanRegistrationAotProcessorTests` (1 of 3 failing test methods: `shouldReplaceResourceProviderCustomizer`) |
