# `*ApplicationContextRunnerTests` lazy CGLIB class-not-found process crash

**Status: FIXED 2026-07-15.** `Class.forName(name, true, loader)` was
correctly resolving Spring's freshly generated `$$SpringCGLIB$$` proxy using
the requested loader, then incorrectly initializing it through a second,
global name lookup. That lookup discarded the resolved loader identity, could
not see the proxy, and let a class-not-found escape as an internal VM error.

`native_class_for_name` now initializes the `ClassId` obtained from the
resolved class mirror directly. This retains the defining loader identity
through initialization and lets Spring's real lazy-proxy generation path run.

While closing the residual exposed by the original class, the native
`BeanDefinitionReaderUtils.registerBeanDefinition` bridge was also corrected
to propagate failures from `registerBeanDefinition`. In particular, duplicate
generated configuration names with bean-definition overriding disabled now
surface their expected `BeanDefinitionOverrideException` instead of being
silently accepted.

## Validation

Built the merged-state isolated release binary
`cratonvm-springboot-lazy-cglib-closure-20260715-build12.exe` and ran each
originally affected class with the Spring Boot test runner in both modes:

| Test class | JIT | `--nojit` |
| --- | ---: | ---: |
| `ApplicationContextRunnerTests` | 28 passed, 0 failed | 28 passed, 0 failed |
| `ReactiveWebApplicationContextRunnerTests` | 28 passed, 0 failed | 28 passed, 0 failed |
| `WebApplicationContextRunnerTests` | 29 passed, 0 failed | 29 passed, 0 failed |

All six executions reported zero failed, aborted, skipped, and failed
containers. The original fatal `$$SpringCGLIB$$` class-not-found process
abort no longer occurs.
