# HTTP client autoconfiguration classpath-presence cluster - FIXED

**Resolved: 2026-07-18**

## Scope

Spring Boot's `@ClassPathExclusions` tests use a
`ModifiedClassPathClassLoader` to hide optional HTTP client libraries. Under
CratonVM, presence checks bypassed that loader and selected the highest-priority
builder even when its library was excluded.

The affected tests were the three exclusion cases in
`ImperativeHttpClientAutoConfigurationTests` and the four exclusion cases in
`ReactiveHttpClientAutoConfigurationTests`. `ClientHttpRequestFactoryBuilder`
also exposed the same defect through its `detect()` overload, which passes a
null loader to `ClassUtils.isPresent`.

## Confirmed root causes

1. CratonVM only honored `ClassLoader.loadClass(String, boolean)` overrides.
   `ModifiedClassPathClassLoader` overrides the public
   `loadClass(String)` overload, so its package exclusion check was skipped.
2. A platform-parented `URLClassLoader` fell back to CratonVM's global
   application classpath after its own URL lookup missed. This resurrected a
   deliberately excluded class or resource.
3. CratonVM's native Spring `ClassUtils.forName(String, ClassLoader)` treated
   a null loader as the global classpath. Spring's contract substitutes the
   current thread context class loader, which is the modified loader while
   these tests run.

## Fix

- Dispatch genuine public `loadClass(String)` overrides once, with a
  reentrancy guard for a subclass's `super.loadClass(name)` call.
- Keep a bootstrap/platform-parented `URLClassLoader` within its recorded URL
  set for class and resource lookup; a local miss is authoritative.
- Resolve a null `ClassUtils.forName` loader through the thread context class
  loader before applying CratonVM's normal built-in-loader path.

## Validation

Fresh release binary:

`C:\craton\CratonVM-target-httpclient-classpath-20260718-019f733b\release\cratonvm.exe`

Spring Boot 4.1.0-SNAPSHOT, JDK 25.0.3:

| Test class | JIT | `--nojit` |
|---|---:|---:|
| `ImperativeHttpClientAutoConfigurationTests` | 8/9 | 8/9 |
| `ReactiveHttpClientAutoConfigurationTests` | 12/13 | 12/13 |
| `ClientHttpRequestFactoryBuilderTests` | 19/19 | 19/19 |
| `ModifiedClassPathExtensionExclusionsTests` | 5/5 | 5/5 |
| `ModifiedClassPathExtensionForkTests` | 1/1 | 1/1 |
| `ModifiedClassPathExtensionForkParameterizedTests` | 3/3 | 3/3 |

The one remaining failure in each autoconfiguration class is the already
separate virtual-thread configuration defect, now fixed in
`docs/internal/springboot/jdk-httpclient-builder-config-loss-cluster-FIXED.md`.
It does not involve classpath presence or `ModifiedClassPathClassLoader`.

The `ModifiedClassPathExtensionOverrides*` failures are a distinct
class-identity/override-URL issue: exclusion/fork behavior passes, while those
tests require an already-loaded Spring class to be redefined from a different
artifact. They are not a false-presence path and are tracked separately in
`docs/known-issues/springboot/modifiedclasspath-override-artifact-identity-regression.md`.
