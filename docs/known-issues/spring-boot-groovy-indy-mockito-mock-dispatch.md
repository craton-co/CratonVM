---
name: spring-boot-groovy-indy-mockito-mock-dispatch
description: OPEN. The last layer of SpringRepositoriesExtensionTests. After fixing the indy guard AIOOBE (3c) and the void-target poly-invoke underflow (3d), the test runs cleanly (3/11 pass, no crashes) but 8 tests fail "expected size N but was 0": a Groovy invokedynamic call (`this.repositories.maven { … }`) on a Mockito mock never drives the stubbed answer, so the repositories list stays empty.
metadata:
  type: known-issue
  area: invoke, groovy, indy, mockito
---

# SpringRepos layer 3e — Groovy indy call on a Mockito mock records no interaction

**Status:** 🔴 OPEN. The **last** blocker for
`org.springframework.boot.build.groovyscripts.SpringRepositoriesExtensionTests`
(Spring Boot buildSrc). Previous layers (1, 2, 3, 3b, 3c, 3d) are all FIXED — see
[[spring-boot-groovy-indy-runtime-argcount-3c-FIXED]] and
[[springrepos-extension-hang-jit-throughput-and-deep-recursion]].

## Where it stands
With the 3c/3d fixes (branch `fix/springrepos-indy-3c`, merged to dev), the test
no longer crashes:

```
JUNIT_RESULT tests=11 passed=3 failed=8 skipped=0 aborted=0
```

The 3 passing are exactly the `assertThat(this.repositories).isEmpty()` cases.
All 8 failures are the same shape:

```
java.lang.AssertionError: Expected size: 3 but was: 0 in: []
  at ...SpringRepositoriesExtensionTests.mavenRepositoriesWhenCommercialSnapshot(...)
```

## Mechanism
`createExtension` (test) builds a **Mockito mock** `RepositoryHandler` and stubs:

```java
RepositoryHandler repositoryHandler = mock(RepositoryHandler.class);
given(repositoryHandler.maven(any(Closure.class))).willAnswer(this::mavenClosure);
```

`mavenClosure` runs the passed closure against a mock `MavenArtifactRepository`
and `this.repositories.add(repository)`. The Groovy under test
(`SpringRepositorySupport.groovy` → `SpringRepositoriesExtension.addRepository`)
does:

```groovy
this.repositories.maven { maven -> maven.setName(name); maven.setUrl(url); … }
```

`this.repositories` is the mock; `maven { … }` is a Groovy **invokedynamic** call
with a `Closure` argument. For the list to fill, that call must reach the mock's
`maven(Closure)` and Mockito's interceptor must record/answer it. It doesn't —
`this.repositories` stays empty, so `mavenClosure` is never invoked (the mock
`repository` would be `add`ed unconditionally even if the closure body no-op'd,
so the answer itself is not firing).

## Candidate root causes (in priority order)
1. **Overload selection.** Gradle's `RepositoryHandler` has both `maven(Closure)`
   and `maven(Action)`. Groovy `selectMethod` may resolve the call to
   `maven(Action)` (closures coerce to `Action`), which is NOT the stubbed
   overload, so Mockito returns the default (`null`) and records nothing the test
   verifies. Check which `maven` overload CratonVM's `Selector`/metaclass picks
   for a `Closure` argument vs HotSpot.
2. **Mockito interception bypass.** A Mockito mock is a ByteBuddy subclass whose
   overridden methods call the `MockMethodInterceptor`. If CratonVM's Groovy-indy
   dispatch invokes the resolved method via a path that bypasses the subclass
   override (e.g. `invokespecial` on the declaring interface/class, or a direct
   metamethod handle bound to the wrong target), the interceptor never runs.
   Verify that a virtual indy dispatch on a Mockito mock hits the ByteBuddy
   override (compare with a plain `mock.maven(closure)` from Java, which works).
3. **Closure → mock argument-matcher mismatch.** `any(Closure.class)` must match
   the actual argument CratonVM passes. If the closure object isn't recognized as
   a `groovy.lang.Closure` instance by the matcher, the stub won't apply.

## How to reproduce
```bash
CV=<cvindy3c.exe or current dev build>
JH="C:/Program Files/Java/jdk-25"
cd apps/spring-boot/buildSrc; CP="runner;$(cat test-classpath.txt)"
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 "$CV" --java-home "$JH" --nojit -cp "$CP" \
  RunJUnit org.springframework.boot.build.groovyscripts.SpringRepositoriesExtensionTests
# expect: tests=11 passed=3 failed=8 (all failures "Expected size: N but was: 0")
```

**Recommended next probe:** a Groovy-free-ish Java/Groovy repro that (a) creates
`mock(RepositoryHandler.class)`, stubs `maven(any(Closure.class))`, and (b)
invokes `mock.maven(someClosure)` once via a Groovy indy call site and once via a
direct Java call — assert both record the interaction. That isolates whether the
miss is overload selection (1/3) or interception bypass (2). Decompile the
relevant `maven` overloads on `RepositoryHandler` (and Gradle's
`ArtifactRepositoryContainer`) to confirm the `Closure` overload exists and its
exact signature.

## Tools / artifacts
- Test: `apps/spring-boot/buildSrc/src/test/.../SpringRepositoriesExtensionTests.java`
- Groovy under test: `apps/spring-boot/buildSrc/SpringRepositorySupport.groovy`
- buildSrc tree is gitignored; runner probes live in `apps/spring-boot/buildSrc/runner/`.
- Indy guard combinators: `native-builtins/src/lang_invoke.rs`
  (`mhs_guard_with_test`, `mh_dispatch` arms, `auto_box_return`).
