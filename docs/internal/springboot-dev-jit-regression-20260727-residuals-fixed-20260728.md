# 2026-07-27 dev JIT regression residuals - fixed 2026-07-28

**Status: CLOSED.** This document was moved from `docs/known-issues` after all
four residual Spring Boot classes passed in both JIT and no-JIT modes.

## Root causes and fixes

1. `NoSpringWebFilterRegistrationBeanTests` and
   `MockWebEnvironmentServletComponentScanIntegrationTests` exposed two parts
   of the annotation materialization defect. Annotation loader calls now pin
   their receiver and temporary class-name String across re-entrant Java work;
   scalar and array enum resolution also use the declaring annotation class's
   loader namespace before a global fallback.
2. `SpringApplicationBuilderTests` was not a remaining VM failure. The original
   watchdog classification was stale: the complete class runs to completion.
3. `SpringApplicationTests` was not hanging. Its five failures came from
   `Class.getGenericInterfaces()` on lambda proxies: the invokedynamic bootstrap
   had recorded the exact functional-interface `ClassId`, but reflection threw
   that identity away and tried to resolve an ambiguous host name. The native
   context now exposes the recorded `ClassId`, which is used before the legacy
   host-scoped/name-only fallbacks. The synthesized `ParameterizedType` is also
   pinned while its result array is allocated.
4. The current-dev ByteBuddy admission was stress-verified on the real JDK
   Mockito initialization path: eight fresh JIT processes with
   `net/bytebuddy/` eligible each completed all 19 tests. Oversize patch
   attempts were rejected before publication; no invalid code was executed.

## Final validation

Executable: `cratonvm-sb-devjit-residuals-r10-20260728-019fa8de.exe`, built
from the topic after merging current `origin/dev`.
Fixture: `C:\craton\CratonVM-spring-boot-rerun-20260717\apps\spring-boot`.
Each class was run in its own process with a 1,200-second timeout.

| Mode | Classes | Tests | Failed | Aborted | Skipped |
| --- | ---: | ---: | ---: | ---: | ---: |
| JIT | 4/4 PASS | 147 | 0 | 0 | 2 |
| no-JIT | 4/4 PASS | 147 | 0 | 0 | 2 |

The two skips are reported by `SpringApplicationTests`; they are expected and
present in both modes. Final result ledgers:

- `C:\craton\sbdevjit-r10-full-jit-20260728-019fa8de\results\residuals-r10-jit\all-jit\results.tsv`
- `C:\craton\sbdevjit-r10-full-nojit-20260728-019fa8de\results\residuals-r10-nojit\all-nojit\results.tsv`
