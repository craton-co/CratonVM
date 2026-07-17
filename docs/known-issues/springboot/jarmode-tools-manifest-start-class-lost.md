# jarmode-tools: `Manifest` main attributes lose `Start-Class` (and possibly other keys) somewhere between parse and read

**Status: OPEN — found 2026-07-17**

## Symptom

| Class | Failures |
|---|---|
| `loader/spring-boot-jarmode-tools` `ExtractCommandTests` | 12 of 14 |
| `loader/spring-boot-jarmode-tools` `IndexedJarStructureTests` | 1 of 6 (`shouldCreateLauncherManifest`) |

```
=> java.lang.IllegalStateException: Manifest attribute 'Start-Class' is mandatory
   org.springframework.util.Assert.state(Assert.java:102)
   org.springframework.boot.jarmode.tools.IndexedJarStructure.getMandatoryAttribute(IndexedJarStructure.java:139)
   org.springframework.boot.jarmode.tools.IndexedJarStructure.createLauncherManifest(IndexedJarStructure.java:123)
```

Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/loader_spring-boot-jarmode-tools.org.springframework.boot.jarmode.tools.IndexedJarStructureTests.out.log`

## Root cause (traced to source, mechanism narrowed but not proven at line level)

`IndexedJarStructureTests.createManifest()` builds a real `java.util.jar.Manifest`
by parsing an in-memory text block via `new Manifest(new
ByteArrayInputStream(...))`:

```java
Manifest-Version: 1.0
Main-Class: org.springframework.boot.loader.launch.JarLauncher
Start-Class: org.springframework.boot.jarmode.tools.IndexedJarStructureTests
Spring-Boot-Version: 3.3.0-SNAPSHOT
Spring-Boot-Classes: BOOT-INF/classes/
Spring-Boot-Lib: BOOT-INF/lib/
Spring-Boot-Classpath-Index: BOOT-INF/classpath.idx
...
```

`IndexedJarStructure`'s constructor (`IndexedJarStructure.java:65-70`)
immediately reads two of the *other* deny-listed attributes from this same
manifest object — `Spring-Boot-Lib` and `Spring-Boot-Classes`, via the same
`getMandatoryAttribute` helper — and **that succeeds** (proven by 5 of 6
`IndexedJarStructureTests` methods passing; every one of them calls
`createStructure()`, whose constructor would throw immediately if those two
reads failed). So the manifest text **is** being parsed correctly in
general, and `Start-Class` **is** present at position 2 of 8 attribute
lines — general manifest-parsing breakage or a truncation/line-count issue
is ruled out.

The one thing that differs for the failing test
(`shouldCreateLauncherManifest`) is that it — uniquely among the passing
tests — calls `IndexedJarStructure.createLauncherManifest()`, whose first
line is:

```java
public Manifest createLauncherManifest(UnaryOperator<String> libraryTransformer) {
    Manifest manifest = new Manifest(this.originalManifest);   // <-- copy constructor
    Attributes attributes = manifest.getMainAttributes();
    for (String denied : MANIFEST_DENY_LIST) {
        attributes.remove(new Name(denied));                    // mutates the COPY
    }
    attributes.put(Name.MAIN_CLASS, getMandatoryAttribute(this.originalManifest, "Start-Class")); // reads the ORIGINAL
    ...
}
```

`getMandatoryAttribute` is called on `this.originalManifest` — the pristine
object, not the copy that gets keys removed — so the deny-list removal
should not be able to affect it under correct `Manifest`/`Attributes`
semantics. The only difference between the passing constructor-time reads
(`Spring-Boot-Lib`/`Spring-Boot-Classes`, both of which run *before* any
`new Manifest(Manifest)` copy is ever made) and the failing
`createLauncherManifest` read (`Start-Class`, read *after* a
`new Manifest(this.originalManifest)` copy-construction happens earlier in
the same method) is the intervening copy-construction call. This points at
`java.util.jar.Manifest`'s (and/or `java.util.jar.Attributes`'s) **copy
constructor** as the suspect: real `Manifest(Manifest man)` does
`this.attr = new Attributes(man.getMainAttributes())`, and real
`Attributes(Attributes attr)` does `this.map = new HashMap<>(attr.map)` — if
CratonVM's `HashMap(Map)` copy-construction path (or the `Attributes.Name`
case-insensitive-hash keying it relies on) has a gap that drops or aliases
some entries when copying *from* another `HashMap`-backed map, that would
explain data loss that is invisible to the earlier, copy-free reads and only
manifests on the object reached through a copy — but this session did not
get as far as instrumenting `HashMap(Map)`/`Attributes(Attributes)`
specifically to confirm it. **Marking as a strong, source-narrowed
hypothesis, not a confirmed file:line fix.** A live repro
(`new Manifest(sourceManifest).getMainAttributes().getValue("Start-Class")`
immediately after construction, both before and after any `remove()` calls)
would confirm or refute this in a few minutes against a live binary.

`ExtractCommandTests`' 12 matching failures go through the identical
`IndexedJarStructure.createLauncherManifest` → `getMandatoryAttribute`
call chain (`ExtractCommand.createApplication` → `createLauncherManifest`),
confirming this is not `IndexedJarStructureTests`-specific test-fixture
noise.

## Affected classes

| Module | Class | Failure count |
|---|---|---|
| `loader/spring-boot-jarmode-tools` | `org.springframework.boot.jarmode.tools.ExtractCommandTests` | 12 of 14 (2 remaining failures are unrelated — see below) |
| `loader/spring-boot-jarmode-tools` | `org.springframework.boot.jarmode.tools.IndexedJarStructureTests` | 1 of 6 |

`ExtractCommandTests`' other 2 failures
(`ExtractLauncher:runWithJarFileThatWouldWriteEntriesOutsideDestinationFails`
— `AssertionError: [Resource] Expecting actual not to be null`) were not
triaged in this pass; not confidently attributed to this bug or any other
cluster in this batch, so left out of any doc rather than guessing.
