# `WebMvcEndpointManagementContextConfigurationTests`: `multipartResolver` factory-method parameter self-resolves to the bean being created

**Status: OPEN — found 2026-07-23 (hypothesis, not confirmed to a CratonVM file:line)**

## Symptom

| Module | Class | Failures |
|---|---|---:|
| `module/spring-boot-webmvc` | `org.springframework.boot.webmvc.autoconfigure.actuate.web.WebMvcEndpointManagementContextConfigurationTests` | 1/3 |

```
JUnit Jupiter:WebMvcEndpointManagementContextConfigurationTests:refreshSucceedsWithoutHealth()
    => java.lang.AssertionError:
Expecting:
 <Unstarted application context ...[startupFailure=org.springframework.beans.factory.UnsatisfiedDependencyException]>
to have not failed:
but context failed to start:
 org.springframework.beans.factory.UnsatisfiedDependencyException: Error creating bean with name 'multipartResolver'
 defined in org.springframework.boot.webmvc.autoconfigure.DispatcherServletAutoConfiguration$DispatcherServletConfiguration:
 Unsatisfied dependency expressed through method 'multipartResolver' parameter 0: Error creating bean with name
 'multipartResolver': Requested bean is currently in creation: Is there an unresolvable circular reference or an
 asynchronous initialization dependency?
 Caused by: org.springframework.beans.factory.BeanCurrentlyInCreationException: Error creating bean with name
 'multipartResolver': Requested bean is currently in creation
```

Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260723/shard1/logs/module_spring-boot-webmvc.org.springframework.boot.webmvc.autoconfigure.actuate.web.WebMvcEndp-ca7e5fc94069.out.log`

## What's happening

Real Spring Boot's `DispatcherServletAutoConfiguration.DispatcherServletConfiguration`
has (verified against the actual Spring Boot 4.1.0-SNAPSHOT source on the
classpath):

```java
@Bean
@ConditionalOnBean(MultipartResolver.class)
@ConditionalOnMissingBean
public MultipartResolver multipartResolver(MultipartResolver resolver) {
    // Detect if the user has created a MultipartResolver but named it incorrectly
    return resolver;
}
```

This is a deliberate "bean-renaming" idiom: the factory method is itself
named `multipartResolver` (so it registers under the canonical Servlet-spec
bean name), and its single parameter is autowired **by type** to find
whatever `MultipartResolver` bean the user (or another autoconfiguration)
actually defined, regardless of what *they* named it. On real Spring, the
by-type autowire candidate search for that parameter must exclude the bean
currently under construction (this factory method's own product) — otherwise
every app using this idiom would deadlock on itself. Real HotSpot passes this
test; the CratonVM run does not, so the exclusion is failing here in some
CratonVM-specific way.

## Root cause — NOT confirmed, two candidate mechanisms

This is **not** the same bug as the already-fixed SPR-8080 reentrancy gap in
`docs/internal/fixed-suite-bugs/CRATONVM-SPRING-GENUINE-BUGLIST.md`
(`origin/dev`, landed `063cd747d`, already an ancestor of this worktree's
HEAD) — that fix covers `native-builtins/src/cglib_enhancer.rs`'s
`emit_bean_override`, i.e. a CGLIB-enhanced `@Configuration` class calling a
**sibling `@Bean` method directly** (`super.otherBean()` / `getBean(name)`
from hand-emitted proxy bytecode). This case is different: `multipartResolver`
takes an autowired **parameter**, resolved through real Spring's own
`DefaultListableBeanFactory.doResolveDependency` → by-type candidate search —
ordinary interpreted/JIT'd Spring bytecode, not a CratonVM-emitted CGLIB
override. Confirmed present as an ancestor commit but evidently doesn't cover
this mechanism.

Two hypotheses, neither traced to a specific file:line this session:

1. **Bean-definition self-inclusion in the by-type candidate scan.** Spring's
   `DefaultListableBeanFactory.doResolveDependency` walks
   `getBeanNamesForType(MultipartResolver.class)` and is supposed to skip the
   name of the bean currently being created (tracked via
   `DefaultSingletonBeanRegistry.singletonsCurrentlyInCreation`, checked
   through `isCurrentlyInCreation(beanName)`). If CratonVM's `HashSet`/`Set`
   membership check for that tracking set behaves incorrectly for this
   specific string (e.g. a hashcode/equals divergence for the bean-name
   `String`, or a bulk/iteration-order bug that skips the exclusion), the
   self-match would leak through exactly like this.
2. **`@ConditionalOnBean(MultipartResolver.class)` self-matching at
   condition-evaluation time.** If the test provides no *other*
   `MultipartResolver` bean, `@ConditionalOnBean` should see zero existing
   beans/definitions of that type and skip registering `multipartResolver`
   entirely — the method should never even become a candidate. If CratonVM's
   condition-evaluation bean-type scan (used heavily elsewhere in this
   codebase's `OnBeanCondition`/`OnClassCondition` native paths) picks up the
   *definition* of `multipartResolver` itself as a qualifying
   `MultipartResolver`-typed bean (a self-referential false positive), the
   method gets registered when it shouldn't, and everything downstream
   follows from there.

Confirming either would need a live debug session: log every
`singletonsCurrentlyInCreation` membership check (or every
`@ConditionalOnBean` type-scan result) for this exact test and see which one
first admits `multipartResolver` where it shouldn't. Not attempted this
session — flagged as the strongest, best-effort hypothesis rather than a
verified root cause per this investigation's scope.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-webmvc` | `org.springframework.boot.webmvc.autoconfigure.actuate.web.WebMvcEndpointManagementContextConfigurationTests` (1 of 3 methods: `refreshSucceedsWithoutHealth`) |

Likely affects any other Spring Boot test that reaches
`DispatcherServletAutoConfiguration.DispatcherServletConfiguration.multipartResolver`
without an explicit user-defined `MultipartResolver` bean — not surveyed
beyond this one class in this session.
