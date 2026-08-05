# Infinispan `ConfigurationBuilder` retransform rejected: "stack overflow during verification"

**Status: OPEN — not reproduced on Windows. Split out 2026-08-05 from
`cacheautoconfigurationtests-configclass-parse-nosuchmethod-gc-20260805`,
whose other two items are fixed and retired
(`docs/internal/fixed-suite-bugs/springboot/cacheautoconfigurationtests-configclass-parse-nosuchmethod-FIXED.md`).**

## Symptom

Observed once, on the Azure Linux full-suite run of 2026-08-05
(`dev @1078f6f05c`), inside `CacheAutoConfigurationTests`:

```
WARN retransformClasses0: UnsupportedClassRedefinitionError {
  class_name: "org/infinispan/configuration/cache/ConfigurationBuilder",
  message: "new bytes failed bytecode verification: verification error in
  org/infinispan/configuration/cache/ConfigurationBuilder.simpleCache:
  at bytecode offset 27: stack overflow during verification" }
```

The test that provokes the retransform is
`CacheAutoConfigurationTests$InfinispanCustomConfiguration.configurationBuilder()`,
which calls `mock(ConfigurationBuilder.class)`; Mockito's inline mock maker
retransforms the class and hands the woven bytes to `redefine_class`, whose
verifier rejects them and rolls the redefine back.

## What is known

* **The offset is not in the original class file.** Both
  `ConfigurationBuilder.simpleCache` overloads are 20 bytes long (`javap -c`
  on `infinispan-core-16.2.1.jar`), so offset 27 exists only in the
  ByteBuddy-woven body. Whatever is being verified is agent-generated.
* **It does not reproduce on Windows.** Not on current `dev`, and not at any of
  the five bisect anchors contemporary with the Azure run
  (`5679ed074`, `6ae882e3f`, `ce5f0ddeb`, `3f0aad96f`, `668907cd3` — all 0–1
  failures for the whole class). With the JIT site-cache defect fixed the class
  is 59/59 green here, so the retransform succeeds and the mock is created.
* **A neighbouring bisect step produced a related face.** `ffd559231` died with
  `java.lang.ArrayIndexOutOfBoundsException` raised *inside* Mockito's inline
  mock creation, before any test ran. Same family — "the bytes ByteBuddy
  produced are not what we then read" — different exception. That step was
  inside the window of the now-fixed JIT defect, so it may already be closed.

## Two hypotheses, and what separates them

1. **Our verifier is wrong** — it over-counts operand-stack depth for some
   construct ByteBuddy emits, and `max_stack` is correctly declared.
2. **The bytes were corrupted before verification** — the woven `byte[]` was
   damaged in flight, and the verifier is correctly rejecting garbage.

Hypothesis 2 was live while the JIT site-cache defect was unfixed, because that
defect corrupted arbitrary reference values. It is much weaker now.

Both diagnostics needed to tell them apart already exist (added 2026-08-05):

* `VerificationFrame::push` now reports the depth, the `max_stack` it would
  exceed, and the stack it was pushing onto — so the message distinguishes an
  under-declared `max_stack` from a verifier that over-counts a category-2
  value. Those have opposite fixes and the bare phrase could not tell them
  apart.
* `CRATONVM_DBG_REDEFINE_DUMP=<dir>` writes the rejected bytes to
  `<dir>/<name>.<n>.class` before the rollback discards them. An
  agent-generated class file is the only copy of what the transformer produced
  and cannot be re-derived by hand, so a rejection was previously
  undisassemblable.

## What to do next

Re-run the Azure full suite on a build that has the site-cache fix, with
`CRATONVM_DBG_REDEFINE_DUMP` set. If it reproduces, `javap -c` the dumped
class: the new verifier message names the depth and limit at offset 27, and the
disassembly says whether `max_stack` is consistent with the body. If it does
not reproduce, this closes with the parent page — but do not close it on a
Windows-only run, since the only sighting is on Linux
([[feedback_a_windows_only_verification_leaves_the_linux_half_uncovered]]).

One confound to rule out first: that Azure checkout carries ~12 788
CRLF-corrupted fixture files (`RESULTS-20260805-azure-fullsuite.md`). Check the
Infinispan jar and the module's test classpath are intact there before treating
a reproduction as a VM defect.

## Affected classes

- `module/spring-boot-cache` — `org.springframework.boot.cache.autoconfigure.CacheAutoConfigurationTests`
  (`infinispanAsJCacheWithConfig`, `infinispanAsJCacheWithCaches`)
