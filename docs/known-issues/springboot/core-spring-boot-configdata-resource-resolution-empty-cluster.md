# `ConfigData`/classpath-and-file resource resolution returns empty for resources that genuinely exist

**Status: OPEN — found 2026-07-17**

## Symptom

6 classes in `core/spring-boot`, well over 100 individual test failures,
all inside Spring Boot's config-data-import machinery
(`org.springframework.boot.context.config.*`). Every failure has the same
shape: a resource (a `.properties`/`.yml` file, or a directory of them,
addressed either as `classpath:...` or a plain/`file:` path) that
unquestionably exists — either shipped as a real classpath test resource
under `src/test/resources`, or created fresh by the test itself in a JUnit
`@TempDir` right before the assertion — is treated by CratonVM as **not
found**, so the property/profile it would have contributed silently never
makes it into the `Environment`.

| Class | Failed/total | Representative symptom |
|---|---:|---|
| `ConfigDataEnvironmentPostProcessorIntegrationTests` | 74/87 | `AssertionFailedError: expected: "fromlocalfile" but was: null` (and ~15 other literal property values); several `ConfigDataLocationNotFoundException: Config data location 'classpath:override.properties' cannot be found` |
| `StandardConfigDataLocationResolverTests` | 17/23 | `AssertionError: Expected size: 1 but was: 0 in: []` for plain (non-wildcard) `file:`/directory resolution; `Expecting code to raise a throwable` for the exception-path tests (nothing throws because nothing ever resolves first) |
| `ConfigDataEnvironmentTests` | 10/21 | `AssertionFailedError: expected: "boot" but was: null`; `Expecting actual: [] to contain exactly: ["one","two","three"]` (active profiles never get set); 1x `ConfigDataLocationNotFoundException: classpath:custom/config.properties` |
| `ConfigDataEnvironmentPostProcessorTests` | 2/6 | `expected: "value" but was: null`; property-source count `Expecting size ... to be greater than 2 but was 2` (no config-data property source ever added) |
| `ConfigDataEnvironmentPostProcessorImportCombinedWithProfileSpecificIntegrationTests` | 2/2 | `expected: "fromicwps1"/"fromicwps2" but was: null` |
| `ConfigDataEnvironmentPostProcessorBootstrapContextIntegrationTests` | 1/1 | `ConfigDataLocationNotFoundException: Config data location 'classpath:imported.properties' cannot be found` |

Representative trace (`ConfigDataEnvironmentPostProcessorBootstrapContextIntegrationTests`):

```
=> org.springframework.boot.context.config.ConfigDataLocationNotFoundException: Config data location 'classpath:imported.properties' cannot be found
   org.springframework.boot.context.config.ConfigDataEnvironment.checkMandatoryLocations(ConfigDataEnvironment.java:400)
   org.springframework.boot.context.config.ConfigDataEnvironment.applyToEnvironment(ConfigDataEnvironment.java:342)
   org.springframework.boot.context.config.ConfigDataEnvironment.processAndApply(ConfigDataEnvironment.java:246)
   org.springframework.boot.context.config.ConfigDataEnvironmentPostProcessor.postProcessEnvironment(ConfigDataEnvironmentPostProcessor.java:97)
   ...
   org.springframework.boot.SpringApplication.run(SpringApplication.java:316)
   org.springframework.boot.context.config.ConfigDataEnvironmentPostProcessorBootstrapContextIntegrationTests.bootstrapsApplicationContext(...:66)
```

And the plain-file-resolution shape (`StandardConfigDataLocationResolverTests`):

```
JUnit Jupiter:StandardConfigDataLocationResolverTests:resolveWhenLocationIsFileResolvesFile()
  => java.lang.AssertionError:
Expected size: 1 but was: 0 in:
[]
     org.springframework.boot.context.config.StandardConfigDataLocationResolverTests.resolveWhenLocationIsFileResolvesFile(StandardConfigDataLocationResolverTests.java:93)

JUnit Jupiter:StandardConfigDataLocationResolverTests:resolveWhenLocationIsWildcardDirectoriesSortsAlphabeticallyBasedOnFixedPath(Path)
  => org.opentest4j.AssertionFailedError:
Expecting actual:
  []
to contain exactly (and in same order):
  ["file [C:\Users\Victor\AppData\Local\Temp\resources1784320060469787800\config\0-empty\testproperties.properties]", ...]
```

Full logs (`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard1/logs/`):
- `core_spring-boot.org.springframework.boot.context.config.ConfigDataEnvironmentPostProcessorIntegrationTests.out.log`
- `core_spring-boot.org.springframework.boot.context.config.StandardConfigDataLocationResolverTests.out.log`
- `core_spring-boot.org.springframework.boot.context.config.ConfigDataEnvironmentTests.out.log`
- `core_spring-boot.org.springframework.boot.context.config.ConfigDataEnvironmentPostProcessorTests.out.log`
- `core_spring-boot.org.springframework.boot.context.config.ConfigDataEnvironmentPostProcessorImp-f8a0e9b8cc6e.out.log`
- `core_spring-boot.org.springframework.boot.context.config.ConfigDataEnvironmentPostProcessorBoo-a8a962f622cd.out.log`

## Root cause

**Confirmed call site, root mechanism not confirmed against CratonVM native
source (hypothesis).** Every one of these tests routes through
`org.springframework.boot.context.config.LocationResourceLoader`
(`apps/spring-boot/core/spring-boot/src/main/java/org/springframework/boot/context/config/LocationResourceLoader.java`),
which `StandardConfigDataLocationResolver` uses to turn a location string
into `Resource` objects:

```java
Resource getResource(String location) {
    ...
    return this.resourceLoader.getResource(location);   // -> Resource
}
Resource[] getResources(String location, ResourceType type) {
    ...
    File[] subDirectories = file.listFiles(this::isVisibleDirectory);
    ...
}
```

and `StandardConfigDataLocationResolver.resolveNonPattern`/`resolvePattern`
(`StandardConfigDataLocationResolver.java:320-339`) gate every resolved
candidate on `resource.exists()`:

```java
Resource resource = this.resourceLoader.getResource(reference.getResourceLocation());
if (!resource.exists() && reference.isSkippable()) {
    logSkippingResource(reference);
    return Collections.emptyList();
}
```

If `resource.exists()` (or, for wildcard/profile-specific lookups,
`File.listFiles(FilenameFilter)`) returns `false`/empty for a resource that
is genuinely present, every downstream consumer sees exactly the observed
symptom: the location is silently treated as absent (if optional — most of
the `expected: "X" but was: null` cases) or raises
`ConfigDataLocationNotFoundException` (if the test marked it mandatory with
`!`/an explicit assertion the location must exist).

This spans **two different underlying resource types**, which is the main
reason the exact CratonVM-side defect could not be pinned this session:

- **`classpath:` locations** (e.g. `classpath:override.properties`,
  `classpath:imported.properties`) resolve through `ClassPathResource`,
  ultimately `ClassLoader.getResource()`/`getResourceAsStream()` →
  `classloading::class_path::ClassPath::find_resource`
  (`classloading/src/class_path.rs:2588`). This function does walk
  `Directory` classpath entries (not just JARs) and does NOT appear, on
  inspection, to reject ordinary filenames like `override.properties` —
  no confirmed defect found in this function itself this session.
- **`file:`/relative locations** (e.g. `resolveWhenLocationIsFileResolvesFile`,
  which resolves a real file under a JUnit `@TempDir`) go through
  `FileUrlResource`/`UrlResource.exists()` → `File.exists()`/`File.listFiles()`,
  a completely different code path from classpath resolution.

Since **both** independent mechanisms show the same "exists() says no"
symptom in the same rerun, the most likely explanation is not a narrow bug
in either resource type individually, but something upstream that both
share — e.g. `ConfigDataEnvironmentContributors`/`ConfigDataLoaders`
catching and silently downgrading an unexpected exception thrown from
*inside* one of these resolution calls (turning a genuine CratonVM-side
`RuntimeException` into "not found" for every location processed in that
pass), or a shared `SpringFactoriesLoader`/`PropertySourceLoader` lookup
gap that makes the whole config-data pipeline behave as if zero loaders/zero
resources are available. **This was not confirmed at file:line precision
this session** — the two concrete Spring-side gates above
(`LocationResourceLoader.getResource`/`getResources`,
`StandardConfigDataLocationResolver.resolveNonPattern`/`resolvePattern`)
are the right place to add `CRATONVM_DBG`-style tracing or a breakpoint to
determine which `exists()`/`listFiles()` call is actually returning the
wrong answer, and whether it's a false-negative in the resource check
itself or an exception being caught upstream.

### Related, but not confirmed to share this root cause

- **`ConfigTreeConfigDataLocationResolverTests.resolveReturnsConfigVolumeMountLocation`**
  (1 failure, same class family) is a **different** symptom — the location
  DOES resolve, but `toString()`s as `"config tree [C:\etc\config\]"`
  (trailing backslash) instead of `"config tree [C:\etc\config]"`. This
  looks like a narrow Windows path-normalization difference (something in
  the config-tree resource's path formatting appends a trailing separator
  that real HotSpot's `Path`/`File` normalization strips), unrelated to the
  "resolution returns empty" mechanism above. Filed here for tracking only,
  not folded into the main cluster.
- **`SpringApplicationBuilderTests`** (2/23 failures — `parentFirstCreationWithProfileAndDefaultArgs`
  expects `"spam"` but gets `null`; `profileAndProperties` expects
  `"profile-specific-file"` but gets `null`) shows the identical
  "profile-specific property file contributed nothing" symptom as the main
  cluster, through the same `SpringApplication.run()` → config-data-import
  pipeline, just exercised via `SpringApplicationBuilder`'s parent/child
  context wiring rather than a direct resolver test. Very likely the same
  root cause; included here as an affected class rather than a separate doc.
- **`BeanDefinitionLoaderTests.loadPackageName`/`loadPackageNameWithoutDot`**
  and **`SimpleMainTests.basePackageScan`** (3 failures total) show
  `IllegalArgumentException: Invalid source 'org.springframework.boot.sampleconfig'`
  / `FileNotFoundException: class path resource [sampleconfig] cannot be
  opened because it does not exist`. This is a **different Spring API**
  (`BeanDefinitionLoader.load(CharSequence)` →
  `PathMatchingResourcePatternResolver`-style package/resource scanning,
  not `LocationResourceLoader`) failing to find classes/resources under the
  `org.springframework.boot.sampleconfig` test package on the classpath —
  same general "classpath resource enumeration comes back empty" symptom
  family, but a structurally different call site. Included here as
  probably-related, not confirmed identical.

## Affected classes

| Module | Class |
|---|---|
| `core/spring-boot` | `org.springframework.boot.context.config.ConfigDataEnvironmentPostProcessorIntegrationTests` |
| `core/spring-boot` | `org.springframework.boot.context.config.StandardConfigDataLocationResolverTests` |
| `core/spring-boot` | `org.springframework.boot.context.config.ConfigDataEnvironmentTests` |
| `core/spring-boot` | `org.springframework.boot.context.config.ConfigDataEnvironmentPostProcessorTests` |
| `core/spring-boot` | `org.springframework.boot.context.config.ConfigDataEnvironmentPostProcessorImportCombinedWithProfileSpecificIntegrationTests` |
| `core/spring-boot` | `org.springframework.boot.context.config.ConfigDataEnvironmentPostProcessorBootstrapContextIntegrationTests` |
| `core/spring-boot` | `org.springframework.boot.builder.SpringApplicationBuilderTests` (related, probably same cause) |
| `core/spring-boot` | `org.springframework.boot.BeanDefinitionLoaderTests` (related, different call site, not confirmed identical) |
| `core/spring-boot` | `org.springframework.boot.SimpleMainTests` (`basePackageScan` only — its other 3 failures belong to the `CapturedOutput` cluster instead, see `capturedoutput-empty-console-cluster.md`) |
| `core/spring-boot` | `org.springframework.boot.context.config.ConfigTreeConfigDataLocationResolverTests` (related but distinct trailing-backslash symptom, see note above) |
