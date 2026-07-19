# MockWebEnvironmentServletComponentScanIntegrationTests — fixed closure

> **UPDATE 2026-07-18 (later same day):** this class regressed again on
> later `dev` tip (a genuine `URLClassLoader.getResourceAsStream`
> real-JDK-mode gap, unrelated to the livelock/loader-URL findings below)
> and was root-caused and fixed — see
> [`urlclassloader-getresourceasstream-real-jdk-mode-dead-FIXED.md`](urlclassloader-getresourceasstream-real-jdk-mode-dead-FIXED.md).
> The findings below are retained for their own evidence trail but are no
> longer the current closure for this class.

The original hang report was consolidated into
[`junit5-interceptingexecutableinvoker-layout-probe-livelock-cluster-FIXED.md`](junit5-interceptingexecutableinvoker-layout-probe-livelock-cluster-FIXED.md)
on 2026-07-18. Its `InterceptingExecutableInvoker` retry-storm root cause is
therefore retired separately.

Revalidating the exact class uncovered and closed the residuals that had been
masked by that former hang:

- Real-JDK `URLClassLoader` construction now preserves each modified loader's
  own URLs in `URLClassPath.path`; the old raw-slot URL stash overwrote the
  real `path` field, so nested JUnit forks could lose their Mockito provider
  resources.
- Nested annotation values retain their declared array shape, and Spring's
  `AnnotationAttributes` adapters preserve `CLASS_TO_STRING` and the empty
  `WebInitParam[]` contract used by servlet component scanning.
- Loader-aware `Class.isAssignableFrom` follows a child definition's resolved
  superclass chain instead of rejecting the corresponding application-loader
  `RegistrationBean` mirror.
- `Constructor.newInstance` passes the declaring mirror's exact `ClassId` to
  the deep-reflection check. Name-only re-resolution is ambiguous after two
  modified class loaders define Mockito classes with the same binary name;
  the second fork then incorrectly rejected construction of the package-private
  `DefaultPluginSwitch` and Mockito reported a `MockResolver` initialization
  failure.

## Verification

Built `cratonvm-mockweb-livelock-019f753a.exe` from the dedicated worktree and
ran `module/spring-boot-web-server`
`MockWebEnvironmentServletComponentScanIntegrationTests` through the Spring
Boot suite runner on 2026-07-18:

| Mode | Result | Tests |
|---|---|---:|
| JIT on | PASS | 3/3 |
| `--nojit` | PASS | 3/3 |

The focused two-fork Mockito reproducer also completed both forks with
`mockingDetails(...).isMock() == true`.
