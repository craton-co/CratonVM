# `PackagedEntityManagerTest` — regressed from documented 15/15 back to 7/15 (2 distinct new bugs isolated, 1 already-known-but-unmerged, 3 uncharacterized)

| | |
|---|---|
| **Status** | 🟡 OPEN (partially). 2 of 8 failures are single-method-reproducible, root-caused, **new** bugs (annotationless XML-only JPA entities not registered; `../../../../apps/META-INF/orm2.xml` not found inside a runtime-built `.par`). 2 of 8 are almost certainly already fixed by an unmerged-at-test-time commit (see below) — reverify before treating as open. 3 of 8 are confirmed genuine failures, not separately root-caused (time-boxed). 1 of 8 is a known harness cleanup artifact, not a bug. |
| **Area** | Hibernate `bootstrap.scanning` — JPA bootstrap from runtime-built `.par` packages (`ShrinkWrap`) added to a fresh child `URLClassLoader`; `MetadataSources`/`ScannedPersistenceUnitInfo` resource+entity discovery. |
| **Discovered** | 2026-07-21, Hibernate ORM JUnit5 suite "passed" category rerun, `apps/hib-suite-runner/run-hib.sh`, binary from worktree `C:\craton\CratonVM-hib-local-0712` (branch `test/hib-local-0712`, merged with `origin/dev` @ `7aed580f0`). |
| **Contradicts** | Commit `08ff46fb0` ("Merge fix/hib-scanning-deephashcode-par", 2026-06-29), whose message explicitly states `PackagedEntityManagerTest 6/15 -> 15/15`, verified against this exact, still-unmodified test file (`git diff 08ff46fb0 -- .../PackagedEntityManagerTest.java` / `PackagingTestCase.java` is empty). |

## Symptom

`run-20260721-175909-passed/on-real/shard-3/raw.log`:

```
@@RESULT 16 org.hibernate.orm.test.bootstrap.scanning.PackagedEntityManagerTest found=15 started=15 ok=7 failed=8 aborted=0 skipped=0 ms=172371
```

Reproduced exactly (same 8 exceptions, same `ok=7 failed=8`) in isolation, **both JIT-on and `--nojit`**:

```
cd C:/craton/CratonVM/apps/hib-suite-runner
echo "org.hibernate.orm.test.bootstrap.scanning.PackagedEntityManagerTest" > /tmp/single-pemt.txt
"C:/craton/CratonVM-hib-local-0712/target/release/cratonvm.exe" --java-home "C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot" --Xmx 1500m @common.args -Dcraton.batch=1 -Dcraton.trace=1 CratonRunner /tmp/single-pemt.txt 0
```

`-Dcraton.trace=1` full stack traces (bottom-most `PackagedEntityManagerTest.java` frame) map each failure to its test method:

| Method | Exception | Resource / entity involved |
|---|---|---|
| `testExcludeHbmPar` (216) | `MappingNotFoundException: META-INF/orm2.xml` | root-level mapping-file inside `excludehbmpar.par` |
| `testDefaultParForPersistence_1_0` (119) | `MappingNotFoundException: org/hibernate/orm/test/jpa/pack/defaultpar_1_0/Mouse.hbm.xml` | nested-path mapping-file inside `defaultpar_1_0.par` |
| `testCfgXmlPar` (253) | `ConfigurationException: Could not locate cfg.xml resource [/org/hibernate/.../hibernate.cfg.xml]` | nested-path `hibernate.cfg.xml` inside `cfgxmlpar.par` |
| `testConfiguration` (397) | `PersistenceException: No Persistence provider for EntityManager named manager1` | `jakarta.persistence.spi.PersistenceProvider` SPI discovery |
| `testExtendedEntityManager` (351) | same as above | same |
| `testListenersDefaultPar` (159/169) | `AssertionFailedError: Failure in default listeners ==> expected: <1> but was: <0>` | `orm.xml` `persistence-unit-defaults/entity-listeners` not invoked |
| `testORMFileOnMainAndExplicitJars` (513/518) | `IllegalArgumentException: 'Seat' is not annotated '@Entity'` | `Seat` — entity declared **only** in `explicitpar/META-INF/orm.xml` |
| `testDefaultPar` (88/99) | `IllegalArgumentException: 'Mouse' is not annotated '@Entity'` | `Mouse` — entity declared **only** in `defaultpar/META-INF/orm.xml` |

(A 9th `@@FAIL` line, `JUnitException: Failed to close extension context`, is `testExtendedEntityManager`'s `@AfterEach` teardown failing because `emf` never got created; not a distinct bug — see "already documented" note below.)

## Regression context

`08ff46fb0` (2026-06-29) fixed `URLClassLoader` `.par`/`.war`/`.ear` archive loading
+ `URI.getSchemeSpecificPart()` `%20` decoding and explicitly claims
`PackagedEntityManagerTest 6/15 -> 15/15`. The test file itself
(`PackagedEntityManagerTest.java`, `PackagingTestCase.java`) has **zero commits**
against it in this repo's history — it is bit-identical to what `08ff46fb0`
verified. Yet the same class now shows `ok=7 failed=8` on current `dev`
(`7aed580f0`). Something regressed in the intervening ~3 weeks. `class_path.rs`,
`net_phase_e.rs`, `classloader.rs`/`classloader_real.rs`, and
`service_loader.rs` all received many further commits in that window (WAR
resource resolution, glob classpath lookup, `getResources()` trailing-slash,
Jandex/hibernate singleton fixes, etc.) — any of these is a plausible
regression vector, but none was bisected to a single commit in this session
(would require building an intermediate binary, out of scope here).

## Minimal single-method repros (rules out cross-test contamination)

Each failing method was re-run **alone** (fresh JUnit `selectMethod`, same
classpath, same JVM invocation) via a throwaway `SingleMethodRunner` launcher
— confirms these are not artifacts of shared static state (`IncrementListener`
counters, TCCL leakage) between the 15 methods of one class run:

```
$ cratonvm.exe ... SingleMethodRunner PackagedEntityManagerTest testDefaultPar
RESULT found=1 ok=0 failed=1
FAIL: java.lang.IllegalArgumentException: Unknown entity type '...Mouse' ('Mouse' is not annotated '@Entity')

$ cratonvm.exe ... SingleMethodRunner PackagedEntityManagerTest testExcludeHbmPar
RESULT found=1 ok=0 failed=1
FAIL: org.hibernate.boot.MappingNotFoundException: Mapping (RESOURCE) not found : META-INF/orm2.xml

$ cratonvm.exe ... SingleMethodRunner PackagedEntityManagerTest testCfgXmlPar
FAIL: ConfigurationException: Could not locate cfg.xml resource [...hibernate.cfg.xml]

$ cratonvm.exe ... SingleMethodRunner PackagedEntityManagerTest testORMFileOnMainAndExplicitJars
FAIL: IllegalArgumentException: 'Seat' is not annotated '@Entity'
```

Each fails identically in complete isolation — these are per-scenario bugs, not
cross-method pollution.

---

## Bug A (root-caused) — annotationless, XML-only JPA entities not registered from `orm.xml` inside a runtime-built `.par`

**`testDefaultPar`**: `defaultpar.par`'s `../../../../apps/META-INF/orm.xml` declares three
entities — `Lighter` (`metadata-complete="true"`), `ApplicationServer`
(`@Entity`-annotated in Java, orm.xml only adds an `<entity-listeners>`
override), and `Mouse` (**zero** annotations in `Mouse.java` — entirely
XML-mapped). The test persists `ApplicationServer` successfully (its table/
sequence DDL runs, per the isolated repro's SQL log), then throws on
`em.persist(mouse)`: `'Mouse' is not annotated '@Entity'`.

**`testORMFileOnMainAndExplicitJars`** shows the identical pattern one level
up: `explicitpar/META-INF/persistence.xml` sets
`exclude-unlisted-classes=true` and lists only `Cat`/`Kitten`/`Distributor`/
`Item` as `<class>` entries (per JPA semantics, `exclude-unlisted-classes`
governs *annotation scanning*, not XML-declared entities — `orm.xml`-declared
entities must still be picked up). `explicitpar/META-INF/orm.xml` declares
`Seat` (`metadata-complete="true"`, zero annotations in `Seat.java`) — fails
with the same `'Seat' is not annotated '@Entity'`.

So: **any entity known to Hibernate purely through an `orm.xml` `<entity>`
element, with no `@Entity` on the class itself, is not making it into the
boot metamodel when that `orm.xml` lives inside a runtime-built `.par` added
to a fresh child `URLClassLoader`.** `orm.xml` itself is clearly being
located and at least partially parsed (the annotated sibling entity in the
same file is registered correctly) — this is not simply "orm.xml not found"
(that's Bug B, below); it's specifically the merge of the XML-only entity
into Hibernate's Jandex/`hibernate-models`-backed metamodel that's dropping
these entries. Not narrowed further to an exact Rust call site in this
session — candidates are the `hibernate-models`/`hibernate-scan-jandex`
boundary (`apps/hibernate-orm/hibernate-scan-jandex/`) or whatever native
scan feeds it a class list for the packaged jar.

## Bug B (root-caused) — root-level `../../../../apps/META-INF/*.xml` mapping-file lookup fails for some runtime-built `.par` archives but not others

**`testExcludeHbmPar`**: `excludehbmpar.par` contains `../../../../apps/META-INF/orm2.xml` at
the jar root (confirmed present both in
`hibernate-core/src/test/bundles/templates/excludehbmpar/META-INF/orm2.xml`
and the built `target/bundles/.../orm2.xml`, and added via
`archive.addAsResource(...)` the same way `defaultpar.par`'s `../../../../apps/META-INF/orm.xml`
is added). `UrlXmlSource.fromResource` → `classLoaderService.locateResource
("META-INF/orm2.xml")` returns `null` → `MappingNotFoundException`. Yet
`defaultpar.par`'s `../../../../apps/META-INF/orm.xml`, added identically (same
`ArchivePaths.create("META-INF/...")` + `archive.addAsResource` pattern, same
root depth), **is** found (see Bug A — orm.xml itself was located; only its
XML-only-entity content was dropped). Also affects `testDefaultParForPersistence_1_0`'s
nested-path `.../Mouse.hbm.xml` and `testCfgXmlPar`'s nested-path
`hibernate.cfg.xml`, both inside their own freshly-built `.par`s.

This does not correlate with a single obvious variable (root vs. nested path;
first `.xml` resource added to the archive vs. second) — `orm2.xml` and
`orm.xml` are both root-level, added the same way, yet one resolves and the
other doesn't, in different `.par` files built by different test methods.
Likely candidate area per `docs/internal/hibernate-bugs/hib-nodepth-shrinkwrap-par-archive-url.md`
(a related, already-fixed bug in this same test suite): CratonVM's real-JDK
`URLClassLoader` does not do genuine per-instance jar scanning — `ucp` is a
bare synthetic `jdk.internal.loader.URLClassPath` and resources are served
from a *global* dynamic classpath shim (`native-builtins/src/classloader.rs`
/ `classloader_real.rs`). A registration/visibility gap in that shim for some
constructor-supplied (as opposed to `addURL`-supplied) `.par` classpath roots
is a plausible root cause, but not confirmed here — would need
`CRATONVM_DIAG_SERVICELOADER`-style tracing (per the sibling doc below) added
to the resource-resolution path, not just SPI discovery.

## `testListenersDefaultPar` — not separately isolated

`IncrementListener.getIncrement()` expected `1`, got `0` — the
`persistence-unit-defaults/entity-listeners` block in `defaultpar/META-INF/orm.xml`
(the same file implicated in Bug A) isn't firing its default `pre-persist`
listener on `ApplicationServer`. Given Bug A already shows this exact
`orm.xml`'s content is only partially applied, this is very likely the same
underlying defect (partial/incomplete JAXB→metamodel merge for `orm.xml`'s
`persistence-unit-defaults` section) rather than a fourth, independent bug —
not confirmed by separate isolation in this session (time-boxed).

---

## Failures NOT new — already root-caused, fix exists but was not yet merged at test time

`testConfiguration` and `testExtendedEntityManager` (`PersistenceException: No
Persistence provider for EntityManager named manager1`) go through
`jakarta.persistence.spi.PersistenceProvider` `ServiceLoader` discovery. This
matches **Bug 1** of
`docs/internal/hibernate-bugs/classloaderserviceimpltest-regressions-20260721-FIXED.md`
(same-day investigation): a plain `file:/C:/...` SPI-descriptor URL (produced
for the *directory* classpath entries in this harness — both
`hibernate-core/target/classes/java/main` and `target/resources/main` are
plain directories on the classpath ahead of `hibernate-core-8.0.0-SNAPSHOT.jar`,
and both contain `../../../../apps/META-INF/services/jakarta.persistence.spi.PersistenceProvider`)
is mishandled by `native-builtins/src/service_loader.rs::discover_providers`
(strips only `"file:"`, keeping a malformed leading slash before the Windows
drive letter; `std::fs::read` fails, silently swallowed).

**That bug's fix, commit `7d893d476` ("fix(hibernate): restore exact class
loader dispatch"), was made 2026-07-21 at 19:52 UTC — after the
`test/hib-local-0712` worktree's merge-base `7aed580f0`, but it IS an ancestor
of current `dev` tip (`cec119210`).** The binary used for this entire
investigation predates that fix. **Reverify `testConfiguration` /
`testExtendedEntityManager` (and the associated `Failed to close extension
context` teardown noise) against a binary rebuilt from current `dev` before
treating them as open** — they are very likely already fixed.
`7d893d476` only touches `classloader.rs`/`service_loader.rs`/`vm_exec.rs`/
`registry.rs`, so it does **not** affect Bugs A/B above (confirmed via
`git log 7aed580f0..cec119210 -- <relevant files>` — no other relevant commits
landed in that window).

## Recommendation

- Do not re-add this class to any "known regression, already fixed" list
  without rebuilding from current `dev` and rerunning — the SPI-provider
  failures are probably gone, Bugs A/B are not.
- Next session: isolate Bug B with `CRATONVM_DIAG_SERVICELOADER=1`-style
  tracing extended to ordinary `getResource`/`getResourceAsStream` (not just
  SPI discovery) to see which classpath-registration path a runtime-built
  `.par`'s root-level XML resources go through, and why `orm2.xml` diverges
  from `orm.xml`. For Bug A, trace `hibernate-scan-jandex`'s indexer /
  `MetadataSources` entity-binding merge to see where the XML-only `Mouse`/
  `Seat` declarations are dropped relative to their annotated siblings.

## Resolution (2026-07-22)

This was one loader-local classpath defect, not separate JPA XML, listener, or
ServiceLoader bugs. `URLClassLoader` retained its constructor URLs correctly,
but its local resolver used `ClassPath::new()`, which admitted only `.jar`
files. Hibernate creates valid ZIP packages with `.par` suffixes, so the child
loader silently omitted every resource and class inside them.

`ClassPath::new()` now treats every existing non-directory file (except its
dedicated JMOD/JImage cases) as an archive candidate, consistent with
`add_path()` and the JDK `URLClassLoader` contract. Invalid files remain safely
ignored because ZIP parsing fails closed. The focused Rust regression
`explicit_non_jar_archive_is_searchable` covers a `.par` ZIP service resource.

Validation used current `dev`, a unique release executable, Temurin
25.0.3.9, and the existing Hibernate fixture:

| Mode | Result |
|---|---|
| JIT | 15/15 passed in 209847 ms |
| `--nojit` | 15/15 passed in 238295 ms |
