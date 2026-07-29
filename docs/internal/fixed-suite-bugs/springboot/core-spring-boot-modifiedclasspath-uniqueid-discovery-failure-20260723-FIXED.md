# Fixed: ModifiedClassPathExtension nested UniqueId discovery

**Fixed 2026-07-29**

## Failure

`ModifiedClassPathExtension` re-runs method-level JUnit discovery with a fresh
`ModifiedClassPathClassLoader`.  Classes reached only through generic
signatures or annotation member values could therefore be resolved from the
first global copy instead of the declaring class's isolated loader.  The mixed
identity graph made nested `selectUniqueId` discovery fail with
`DiscoveryIssueException` for every method in affected Spring Boot classes.

## Fix

Generic signature resolution now first resolves a referenced class through the
declaring class's loader, before considering the already-registered or global
copy.  Annotation proxy/member-value resolution likewise retains the declaring
loader, including generated Ehcache JAXB model classes whose direct loader
record is absent.  This preserves the exact child-loader class identity across
nested JUnit discovery, generic reflection, and annotation values.

## Validation

Fixture: `C:\sbmcu28` at `55a520e`; executable:
`cratonvm-modifiedclasspath-uniqueid-20260728-019fa9-r14-release-no-lto.exe`
(SHA-256 `57C97AA02A2865910AA5A1BA4A62B1355D889CD7F0D8465DD73558DB7074200D`).

The authoritative 28-class closure manifest
`apps/spring-boot-suite-runner/modifiedclasspath-uniqueid-closure-20260728-019fa9.tsv`
completed in both modes:

| Mode | Classes | Tests | Failures | Aborted | Skipped | Container failures |
|---|---:|---:|---:|---:|---:|---:|
| JIT | 28/28 PASS | 315 | 0 | 0 | 0 | 0 |
| no-JIT | 28/28 PASS | 315 | 0 | 0 | 0 | 0 |

The original 15 modified-classpath discovery classes and their related closure
residuals are covered by this manifest.  This supersedes the open issue record.
