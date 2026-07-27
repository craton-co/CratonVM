# `BeanDefinitionLoader.findPackage()`'s classpath directory scan finds nothing under the suite runner's pathing jar

**Status: OPEN — found 2026-07-23 (hypothesis, not fully pinned to file:line)**

## Symptom

```
java.lang.IllegalArgumentException: Invalid source 'org.springframework.boot.sampleconfig'
	at org.springframework.boot.BeanDefinitionLoader.load(BeanDefinitionLoader.java:204)
	at org.springframework.boot.BeanDefinitionLoader.load(BeanDefinitionLoader.java:149)
	at org.springframework.boot.BeanDefinitionLoader.load(BeanDefinitionLoader.java:130)
	at org.springframework.boot.SimpleMainTests.basePackageScan(SimpleMainTests.java:53)
```

Also `BeanDefinitionLoaderTests.loadPackageName()` (same exception, same
site) and `.loadPackageNameWithoutDot()` (a related
`BeanDefinitionStoreException: IOException parsing XML document from class
path resource [sampleconfig]` — `Caused by: FileNotFoundException: class
path resource [sampleconfig] cannot be opened because it does not exist`).

Both classes are attempting to load Spring bean definitions from a
**package name** (`org.springframework.boot.sampleconfig`, or bare
`sampleconfig`), a legitimate `SpringApplication.run(Object...)` source
kind. The target packages genuinely exist and contain real, compiled test
classes:

```
apps/spring-boot/core/spring-boot/src/test/java/org/springframework/boot/sampleconfig/MyComponent.java
apps/spring-boot/core/spring-boot/src/test/java/org/springframework/boot/sampleconfig/MyNamedComponent.java
apps/spring-boot/core/spring-boot/src/test/java/org/springframework/boot/sampleconfig/package-info.java
apps/spring-boot/core/spring-boot/src/test/java/sampleconfig/MyComponentInPackageWithoutDot.java
apps/spring-boot/core/spring-boot/src/test/java/sampleconfig/package-info.java
```

Full logs:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260723/shard1/logs/core_spring-boot.org.springframework.boot.BeanDefinitionLoaderTests.out.log`
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260723/shard4/logs/core_spring-boot.org.springframework.boot.SimpleMainTests.out.log`

## Root cause (hypothesis)

`BeanDefinitionLoader.load(CharSequence source)`
(`apps/spring-boot/core/spring-boot/src/main/java/org/springframework/boot/BeanDefinitionLoader.java:184-205`)
tries, in order: as a `Class` (fails, not a class name), as classpath
resources (fails, no direct resource matches the bare package name), then
`findPackage(source)`
(`BeanDefinitionLoader.java:259-281`):

```java
private @Nullable Package findPackage(CharSequence source) {
    Package pkg = getClass().getClassLoader().getDefinedPackage(source.toString());
    if (pkg != null) return pkg;
    try {
        // Attempt to find a class in this package
        ResourcePatternResolver resolver = new PathMatchingResourcePatternResolver(getClass().getClassLoader());
        Resource[] resources = resolver.getResources(ClassUtils.convertClassNameToResourcePath(source.toString()) + "/*.class");
        for (Resource resource : resources) {
            ... load(Class.forName(source + "." + className)); break;
        }
    } catch (Exception ex) { /* swallow */ }
    return getClass().getClassLoader().getDefinedPackage(source.toString());
}
```

The package is never yet "defined" on the classloader (no class from it
has loaded), so the first check misses; the fallback lists `*.class` files
under the package's classpath directory via
`PathMatchingResourcePatternResolver.getResources(...)` and loads the first
one found (which would define the package as a side effect). If that
directory listing comes back **empty**, `findPackage` returns `null` twice
over and `load()` falls through to `throw new IllegalArgumentException("Invalid
source '" + resolvedSource + "'")` — exactly the observed failure.

This suite runner routes the test classpath through a **manifest-only
"pathing jar"** to sidestep Windows' command-line length limit — visible in
a sibling failure's dumped `java.class.path`:
`C:\craton\...\pathing-jars\core_spring-boot-b690bc8e099d0e42.jar` (a
single jar whose `MANIFEST.MF` `Class-Path:` attribute lists the real
classpath entries, including the actual `.../test/java` output directory
that contains `sampleconfig/`). Real HotSpot resolves this indirection
transparently for `ClassLoader.getResources()`/directory listing (that's
the whole point of a pathing jar). The working hypothesis is that
CratonVM's classpath resource enumeration for a directory pattern does not
fully honor a single-jar `Class-Path:` manifest indirection the way
`URLClassLoader` does on HotSpot, so
`PathMatchingResourcePatternResolver.getResources(pkgPath + "/*.class")`
comes back empty for these two package-name-based tests even though the
`.class` files are genuinely present and reachable (other tests in the
same run load individual classes from this same classpath without issue,
which is consistent with single-file lookups working while *directory
listing* through the pathing-jar indirection does not).

Not confirmed to a CratonVM file:line — this needs isolating from the
suite runner's pathing-jar mechanism (e.g. rerun with a real, unshortened
`-cp` argument instead of the pathing jar) to see whether the failure
persists, which would point the blame back at Spring/`PathMatchingResourcePatternResolver`
instead.

**Note:** this class-loading gap is *not* inside `loader/spring-boot-loader`
(that module is out of scope for this investigation) — the pathing jar is
purely a suite-runner/build convention, unrelated to Spring Boot's own
executable-jar `Launcher`/`Archive` machinery.

## Affected classes

| Module | Class |
|---|---|
| core/spring-boot | org.springframework.boot.BeanDefinitionLoaderTests (2 of 13 failures: `loadPackageName`, `loadPackageNameWithoutDot`) |
| core/spring-boot | org.springframework.boot.SimpleMainTests (1 of 5 failures: `basePackageScan`) |
