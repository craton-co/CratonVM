# Fixed 2026-07-01: spring-web `RestClientExtensionsTests` mockk `verify{}` PTR args

## Resolution 2026-07-01

Current `dev` no longer reproduces this issue. The original report below was
verified failing on `3cc5bf29`; the current workspace at `89db75ae` passes the
same single-class repro in real-JDK, JIT-on mode.

Verification:

```
cd apps/spring-suite-runner
CRATONVM_BIN=/c/craton/CratonVM/target/release/cratonvm.exe \
  ./run-suite.sh run --jdk real --jit on --batch 1 \
  --only 'web\.client\.RestClientExtensionsTests' --tag codex-verify
# RESULT: OK, found=5, passed=5, failed=0, ms=83478
```

Negative-control check with cross-thread JIT root takeover disabled also passes,
so the adjacent `dfd3d560` takeover wait change is not required for this class:

```
CRATONVM_BIN=/c/craton/CratonVM/target/release/cratonvm.exe \
CRATONVM_XT_JIT_ROOT_SCAN=0 \
  ./run-suite.sh run --jdk real --jit on --batch 1 \
  --only 'web\.client\.RestClientExtensionsTests' --tag codex-verify-noxt
# RESULT: OK, found=5, passed=5, failed=0, ms=69494
```

The exact causal commit was not isolated from the post-report `dev` movement, but
the suite behavior is fixed on current `dev`. The historical report is retained
below for traceability.

## Original Report

# spring-web `RestClientExtensionsTests` — mockk `verify{}` rejects structurally-equal `ParameterizedTypeReference` args

**Status:** OPEN (3/5) · **Mode:** real-JDK, JIT on · HotSpot passes 5/5
**Dev verified on:** `3cc5bf29`

## Symptom

`org.springframework.web.client.RestClientExtensionsTests` — Kotlin + mockk. 3 of 5
methods fail with:

```
java.lang.AssertionError: Verification failed: call 2 of 2:
RequestBodySpec(child of #..).body(eq(List(#..)), eq(ParameterizedTypeReference<java.util.List<? extends …Foo>>))).
Only one matching call to …/body(Any, ParameterizedTypeReference) happened, but arguments are not matching:
[1]: argument: ParameterizedTypeReference<…List<? extends …Foo>>,
     matcher: eq(ParameterizedTypeReference<…List<? extends …Foo>>), result: -
```

The recorded argument and the `eq(...)` matcher render **identically**, yet mockk reports no
match. Failing: `RequestBodySpec#body`, `ResponseSpec#toEntity`, `ResponseSpec#requiredBody`.
Passing: `ResponseSpec#body`, `ResponseSpec#requiredBody with null…`.

## NOT the "Can't instantiate proxy" bug

The original failure of this class — all 5 methods throwing
`io.mockk.MockKException: Can't instantiate proxy for class RestClient$RequestBodySpec`
(root: `AnnotatedTypeFactory$AnnotatedTypeBaseImpl.getAnnotatedOwnerType()` NPE on a null
`location`) — is **FIXED** by the AnnotatedType `location` seed on dev (`3ee5ffaf`,
`annotated_type_fill_bookkeeping`), which the frozen suite binary predates. With that fix the
class goes 0/5 → 2/5; the 3 residual failures below are a *different* bug.

## What it is NOT (ruled out)

- **NOT `ParameterizedType`/`WildcardType` equality.** Reflectively instantiating the two actual
  anonymous PTR subclasses (`…$$inlined$body$1` vs the explicit `…$1`) and comparing gives
  `getType().equals(...) == true` with equal hashCodes on CratonVM, matching HotSpot. All the
  inlined `ParameterizedTypeReference<T>` classes carry an identical class `Signature`
  (`…ParameterizedTypeReference<Ljava/util/List<+L…Foo;>;>;`).
- **NOT identity-hashCode magnitude.** CratonVM's `System.identityHashCode` was small sequential
  (1,2,3,…; `gc::next_hash` is a bare counter) vs HotSpot's large spread values. Patching
  `next_hash` with golden-ratio mixing made the hashes HotSpot-like (±2.1e9, 0 collisions) — but
  the class stayed 2/5. Reverted (no benefit).

## What is known

- Reproduces ONLY under JUnit (`@Test` + field mocks), never via a plain `main()` — even a
  reflectively-invoked method with a nested private `Foo` passes.
- The recorded PTR **is equal** to a fresh one: captured via `every{…} answers { secondArg() }`,
  `recorded == fresh == true`; and a `verify { …body(any(), match { it == fresh }) }` PASSES. Only
  mockk's auto-`eq` on the same literal fails.
- Discriminator: `body(any(), literalPTR)` passes; the real `body(literalMock, literalPTR)` (a
  ByteBuddy-proxy **mock** as the first literal arg, beside the PTR literal) fails — but only in
  some test structures (full class / field mocks), not others. Deterministic but
  structure/allocation-order-dependent inside mockk's relaxed-verify arg-signature matching
  (`io.mockk.impl.recording.SignatureMatcherDetector` / `JvmSignatureValueGenerator`).

Remaining suspicion: mockk's signature/arg detection mis-aligns the PTR arg when a proxy mock is
also a literal arg, sensitive to some CratonVM reflection/ByteBuddy/proxy behaviour. Pinning it
needs mockk-source-level instrumentation.

## Repro

```
cd apps/spring-suite-runner
CRATONVM_BIN=<vm> ./run-suite.sh run --jdk real --jit on --batch 1 \
  --only 'web\.client\.RestClientExtensionsTests'
# expect FAIL 3/5; HotSpot: ./run-suite.sh hotspot --only 'web\.client\.RestClientExtensionsTests' -> 5/5
```

Minimal Kotlin repros (compiled with `kotlin-compiler-embeddable-2.3.20.jar`, run via KRun)
reproduce a single failing `body(mock, PTR)` case under JUnit; a `main()` version of the same
calls passes.
