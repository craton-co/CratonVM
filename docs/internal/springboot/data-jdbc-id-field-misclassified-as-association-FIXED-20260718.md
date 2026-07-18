# Spring Data JDBC: a plain `@Id private Long id` field is misclassified as an "association", `Association.from()` throws `IllegalArgumentException`

**Status: FIXED — retired 2026-07-18. The original investigation below is retained for diagnostic history.**

## Resolution (2026-07-18)

**Status: FIXED / record retired.** The current VM correctly reports
`org.jmolecules.ddd.types.Association` as absent when it is not on the
classpath. In particular, both `Class.forName(name, false, loader)` and the
underlying `ClassLoader.loadClass(name)` throw `ClassNotFoundException` rather
than producing a class-like result. Spring Data therefore leaves its optional
`ASSOCIATION_TYPE` gate null and does not classify ordinary `Long`, `String`,
or `Name` fields as associations.

This was verified with the exact absent jMolecules name on CratonVM with JIT
enabled and with `--nojit`, alongside a real JDK 25 control. The focused
`ROptionalClassForName` regression now protects both API forms. A short
Windows suite attempt against the historical fixture did not reproduce the
old `Association.from()` exception before its independent 55-second timeout;
that timeout is not treated as evidence for this resolved classification bug.

No new VM implementation change was required because `dev` already has the
correct optional-class contract. The original 2026-07-17 report is retained
below for diagnostic history; the affected JDBC, Cassandra, and LDAP
manifestations share the same retired jMolecules gate.

## Original report

## Symptom

`module/spring-boot-data-commons`'s `DataRepositoryMetricsAutoConfigurationIntegrationTests`
fails 1 of its 2 tests:

```
JUnit Jupiter:DataRepositoryMetricsAutoConfigurationIntegrationTests:repositoryMethodCallRecordsMetrics()
    => java.lang.IllegalArgumentException: Cannot determine reference type type for @org.springframework.data.annotation.Id()private java.lang.Long org.springframework.boot.data.domain.city.City.id
       org.springframework.data.jdbc.core.convert.Association.from(Association.java:74)
       org.springframework.data.jdbc.core.convert.SqlGenerator$Columns.lambda$populateColumnNameCache$0(SqlGenerator.java:1407)
       org.springframework.data.mapping.PersistentEntity.lambda$doWithAll$0(PersistentEntity.java:294)
       org.springframework.data.mapping.model.BasicPersistentEntity.doWithAssociations(BasicPersistentEntity.java:375)
       org.springframework.data.mapping.PersistentEntity.doWithAll(PersistentEntity.java:293)
       org.springframework.data.jdbc.core.convert.SqlGenerator$Columns.populateColumnNameCache(SqlGenerator.java:1403)
       org.springframework.data.jdbc.core.convert.SqlGenerator$Columns.<init>(SqlGenerator.java:1384)
       org.springframework.data.jdbc.core.convert.SqlGenerator.<init>(SqlGenerator.java:130)
       org.springframework.data.jdbc.core.convert.SqlGeneratorSource.lambda$getSqlGenerator$0(SqlGeneratorSource.java:71)
       ...
       org.springframework.data.jdbc.core.convert.DefaultDataAccessStrategy.count(DefaultDataAccessStrategy.java:284)
       org.springframework.data.jdbc.core.JdbcAggregateTemplate.count(JdbcAggregateTemplate.java:317)
       org.springframework.data.jdbc.repository.support.SimpleJdbcRepository.count(SimpleJdbcRepository.java:105)
```

Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/module_spring-boot-data-commons.org.springframework.boot.data.autoconfigure.metrics.DataReposi-3d579b8670ec.out.log`

**Same signature also hits `module/spring-boot-data-jdbc`'s own
`DataJdbcRepositoriesAutoConfigurationTests`** (2 of 15 tests:
`basicAutoConfiguration`, `honoursUsersEnableJdbcRepositoriesConfiguration`),
against the same `City` fixture class (a separate copy under
`org.springframework.boot.data.jdbc.domain.city.City`, same shape — plain
`@Id private Long id` plus `String name/state/country/map`). One of the two
failures there is on `id` (`Long`) exactly as above; the other is on
`country` (`String`), confirming the misclassification is not specific to
`@Id`/`Long` — a plain non-`@Id` `String` field trips the identical
`Association.from()` throw:

```
java.lang.IllegalArgumentException: Cannot determine reference type type for private java.lang.String org.springframework.boot.data.jdbc.domain.city.City.country
       org.springframework.data.jdbc.core.convert.Association.from(Association.java:74)
       (same call chain, via DefaultDataAccessStrategy.findById → SqlGenerator$Columns.populateColumnNameCache)
```

Full log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/module_spring-boot-data-jdbc.org.springframework.boot.data.jdbc.autoconfigure.DataJdbcReposito-dc5ae8ae98b0.out.log`

This strengthens hypothesis 1 below (a `Class`-identity/equality problem
against `SimpleTypeHolder`'s registered simple-type set) over hypothesis 2
(something `@Id`-specific): `country` carries no `@Id` annotation at all,
yet is misclassified the same way, so whatever breaks `SimpleTypeHolder`'s
recognition of "this is a simple scalar type" is not annotation-driven —
it reproduces for `java.lang.Long` and `java.lang.String` alike, i.e. for
plain wrapper/String field types generally, regardless of annotations.

## Root cause (partially confirmed control flow; deeper cause is a hypothesis)

**Confirmed via `javap -c -l` on the real `spring-data-jdbc-4.1.0-RC1.jar`**
(`org/springframework/data/jdbc/core/convert/Association.class`): `Association.detect(property,
converter)` only ever calls `Association.from(...)` — the method that
throws — when `Association.isAssociation(property)` has **already returned
`true`** for that property:

```java
public static boolean isAssociation(RelationalPersistentProperty property) {
    return property.isAssociation() && !property.isQualified();
}
public static Association detect(RelationalPersistentProperty property, JdbcConverter converter) {
    return isAssociation(property) ? from(property, converter) : null;
}
```

So for `Association.from()` to be reached at all for `City.id` — a plain
`@Id private Long id` field with no `@MappedCollection`/reference semantics
— Spring Data's own `RelationalPersistentProperty.isAssociation()` must be
returning `true` for it. On real HotSpot, wrapper-type fields like `Long`
are classified via `SimpleTypeHolder`/`AnnotationBasedPersistentProperty`
as "simple" (not an association/entity reference), so `isAssociation()`
returns `false` and `PersistentEntity.doWithAssociations` never visits the
`id` field in the first place — matching the fact that this passes on the
same-scope real-HotSpot baseline.

**The deeper cause of why CratonVM causes `isAssociation()` to return
`true` for `java.lang.Long` here is not confirmed this session** — no trace
of `AnnotationBasedPersistentProperty.isAssociation()`/`SimpleTypeHolder`
was done. Plausible candidate mechanisms, none verified:

1. A reflection/generic-signature resolution gap causes the property's
   resolved `Class`/`TypeInformation` for `Long` to not compare `.equals()`/
   `==` correctly against `SimpleTypeHolder`'s registered `Long.class`
   entry (e.g. two distinct `Class` mirror objects for the same type from
   different resolution paths — a pattern seen elsewhere in this project's
   reflection/generics bugs).
2. Something specific to `@Id`-annotated fields causes Spring Data's
   annotation-driven association detection (which for JDBC treats a field
   as an association when its type is itself recognized as another
   persistent entity, or carries certain annotations) to see stale/wrong
   annotation metadata and treat the (wrapper-typed, `@Id`) field as if it
   pointed to another aggregate root.

Neither hypothesis is pinned to a CratonVM source file/line. Confirming
would require either a standalone probe (build a `RelationalMappingContext`,
get the `PersistentEntity` for a simple `@Id Long`-only class, call
`.isAssociation()` on the id property, and diff against real JDK 25), or a
breakpoint/print inside `AnnotationBasedPersistentProperty.isAssociation()`
during this exact test.

## Update 2026-07-17 (bin13 rerun triage) — likely same upstream mechanism, a third module, different downstream symptom

`module/spring-boot-data-ldap`'s `DataLdapRepositoriesAutoConfigurationTests`
fails 2 of its 3 tests with a shape that looks like the **same upstream
cause manifesting through a different Spring Data submodule's downstream
handling**:

```
JUnit Jupiter:DataLdapRepositoriesAutoConfigurationTests:testDefaultRepositoryConfiguration()
    => org.springframework.beans.factory.BeanCreationException: ...: Cannot create PersistentEntity for 'org.springframework.boot.data.ldap.autoconfigure.domain.person.Person'
     Caused by: org.springframework.data.mapping.MappingException: Cannot create PersistentEntity for '...Person'
     Caused by: java.lang.UnsupportedOperationException: LDAP does not support associations
       org.springframework.data.ldap.core.mapping.LdapPersistentProperty.createAssociation(LdapPersistentProperty.java:51)
       org.springframework.data.mapping.model.AbstractPersistentProperty.lambda$new$0(AbstractPersistentProperty.java:89)
       org.springframework.data.mapping.context.AbstractMappingContext$PersistentPropertyCreator.createAndRegisterProperty(AbstractMappingContext.java:660)
```

The mapped entity
(`apps/spring-boot/module/spring-boot-data-ldap/src/test/java/.../domain/person/Person.java`)
is:

```java
@Entry(objectClasses = { "person", "top" }, base = "ou=someOu")
public class Person {
    @Id
    private @Nullable Name dn;          // javax.naming.Name — registered as a
                                         // simple type by Spring LDAP's mapping
                                         // context specifically so the @Id
                                         // distinguished-name property isn't
                                         // treated as an association
    @Attribute(name = "cn")
    @DnAttribute(value = "cn", index = 1)
    private @Nullable String fullName;
}
```

`LdapPersistentProperty.createAssociation()` unconditionally throws — it's
only reached once Spring Data's shared `AbstractMappingContext`/
`AbstractPersistentProperty` machinery has already classified the property
as an association, same generic upstream gate as `RelationalPersistentProperty.isAssociation()`
above. Since `javax.naming.Name` (an interface) and `java.lang.Long`/
`java.lang.String` (concrete wrapper/String types) are unrelated types
whose only common trait is "registered as simple/not-an-association by
each submodule's own `SimpleTypeHolder`-style configuration", this is
consistent with hypothesis 1 above (a `Class`-identity/equality or
`isAssignableFrom` gap against each module's registered simple-type set) —
whatever check is failing does not appear to be type- or annotation-shape
specific. Not independently confirmed against `AbstractMappingContext`/
`SimpleTypeHolder` source this round either — filed here rather than as a
separate doc because the shared "some property that real HotSpot resolves
as simple gets misclassified as an association, and the downstream
Spring Data submodule's own no-association handling then throws" shape
is the same open question, just surfacing through LDAP's own
`createAssociation()` override instead of JDBC's `Association.from()`.
Full log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/module_spring-boot-data-ldap.org.springframework.boot.data.ldap.autoconfigure.DataLdapReposito-2a8e86c0bbb5.out.log`

## Update 2026-07-17 (bin7 rerun triage) — hypothesis 1 strongly confirmed at the bytecode level: `AbstractPersistentProperty`'s `ASSOCIATION_TYPE` (jMolecules DDD) gate

Independently found and traced (before discovering this doc) the same
misclassification against `module/spring-boot-data-cassandra` (`City` —
plain `Long`/`String` fields, no `@Id`-specificity, matching this doc's own
`country`-field evidence) and `module/spring-boot-data-ldap-test` (a
`@DataLdapTest`-scoped module, distinct from this doc's plain
`module/spring-boot-data-ldap`, but the same `LDAP does not support
associations` symptom against a different `Name`-typed `@Id` field,
`ExampleEntry.dn`):

| Class | Symptom |
|---|---|
| `DataCassandraAutoConfigurationTests` (1/9), `DataCassandraReactiveAutoConfigurationTests`, `DataCassandraReactiveRepositoriesAutoConfigurationTests`, `DataCassandraRepositoriesAutoConfigurationTests` | `UnsupportedOperationException: Cassandra does not support associations` at `BasicCassandraPersistentProperty.getAssociation` |
| `DataLdapTestIntegrationTests`, `DataLdapTestPropertiesIntegrationTests` (+ `NestedTests`), `DataLdapTestWithIncludeFilterIntegrationTests` | `UnsupportedOperationException: LDAP does not support associations` at `LdapPersistentProperty.getAssociation`, same as this doc's `spring-boot-data-ldap` entry but a different module/entity (`ExampleEntry.dn`) |

Full logs: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/module_spring-boot-data-cassandra.*.out.log`,
`.../module_spring-boot-data-ldap-test.*.out.log`.

**Decompiled `spring-data-commons-4.1.0-RC1.jar`'s
`AbstractPersistentProperty.class` pins down exactly what "hypothesis 1"
(a `Class`-identity/equality gap) actually is.** Every store's
`isAssociation()` (JDBC's `RelationalPersistentProperty`, LDAP's, Cassandra's
— all inherit this base implementation) is a `Lazy<Boolean>` whose supplier
(`lambda$new$2`, decompiled bytecode):

```java
private Boolean lambda$new$2() {
    if (ASSOCIATION_TYPE == null) return false;
    return ASSOCIATION_TYPE.isAssignableFrom(this.rawType);
}
```

`ASSOCIATION_TYPE` is a `static final Class<?>` set once in
`AbstractPersistentProperty`'s `<clinit>` (decompiled):

```java
static {
    CAUSE_FIELD = ReflectionUtils.getRequiredField(Throwable.class, "cause");
    ASSOCIATION_TYPE = ClassUtils.loadIfPresent(
        "org.jmolecules.ddd.types.Association",
        AbstractPersistentProperty.class.getClassLoader());
}
```

`ClassUtils.loadIfPresent` (also decompiled) calls `Class.forName(name,
loader)` inside a catch-all `try { ... } catch (Exception e) { return null;
}` — designed to silently return `null` when the optional jMolecules DDD
library isn't on the classpath. **Confirmed**: `org.jmolecules.ddd.types.Association`
is genuinely absent from every affected module's generated test classpath
(checked `build/cratonvm-test-cp.txt` for `spring-boot-data-cassandra` and
`spring-boot-data-ldap-test` — no `jmolecules*` entry). On real HotSpot this
means `Class.forName` throws `ClassNotFoundException`, `loadIfPresent`
returns `null`, `ASSOCIATION_TYPE == null`, and `isAssociation()`
short-circuits to `false` for **every** property, regardless of type — this
is why HotSpot passes and why the bug reproduces identically for
`java.lang.Long`, `java.lang.String`, and `javax.naming.Name` alike (this
doc's own observation, now explained: none of these types are "specially"
misclassified — the gate that's supposed to reject *all* of them is itself
broken).

Since every property trips this identically regardless of type, the most
likely explanation is that `ASSOCIATION_TYPE` ends up **non-null** under
CratonVM (either `Class.forName` for a genuinely-absent class name returns
a non-null stub instead of throwing, or an earlier step in
`org.springframework.util.ClassUtils.forName`'s own dispatch resolves the
dotted name to something unintended) — not yet distinguished by a live
repro (this session did not build/run the VM). **Confirms/refines this
doc's hypothesis 1, not hypothesis 2** — the gate that's broken
(`ASSOCIATION_TYPE`/`ClassUtils.loadIfPresent`) has nothing to do with
`@Id` annotations specifically, matching this doc's own `country`-field
(non-`@Id` `String`) evidence exactly.

**What would confirm/refute:** a standalone probe —
`ClassUtils.loadIfPresent("org.jmolecules.ddd.types.Association",
SomeClass.class.getClassLoader())` run directly against a CratonVM binary
with no Spring context — dumping whether the return value is `null` (as it
must be) or some resolved `Class`, and if resolved, its actual identity.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-data-commons` | `org.springframework.boot.data.autoconfigure.metrics.DataRepositoryMetricsAutoConfigurationIntegrationTests` (1 of 2 tests) |
| `module/spring-boot-data-cassandra` | `org.springframework.boot.data.cassandra.autoconfigure.DataCassandraAutoConfigurationTests` (1/9, added bin7) |
| `module/spring-boot-data-cassandra` | `org.springframework.boot.data.cassandra.autoconfigure.DataCassandraReactiveAutoConfigurationTests` (added bin7) |
| `module/spring-boot-data-cassandra` | `org.springframework.boot.data.cassandra.autoconfigure.DataCassandraReactiveRepositoriesAutoConfigurationTests` (added bin7) |
| `module/spring-boot-data-cassandra` | `org.springframework.boot.data.cassandra.autoconfigure.DataCassandraRepositoriesAutoConfigurationTests` (added bin7) |
| `module/spring-boot-data-ldap-test` | `org.springframework.boot.data.ldap.test.autoconfigure.DataLdapTestIntegrationTests` (added bin7, distinct module from `spring-boot-data-ldap` above) |
| `module/spring-boot-data-ldap-test` | `org.springframework.boot.data.ldap.test.autoconfigure.DataLdapTestPropertiesIntegrationTests` (added bin7) |
| `module/spring-boot-data-ldap-test` | `org.springframework.boot.data.ldap.test.autoconfigure.DataLdapTestWithIncludeFilterIntegrationTests` (added bin7) |
| `module/spring-boot-data-jdbc` | `org.springframework.boot.data.jdbc.autoconfigure.DataJdbcRepositoriesAutoConfigurationTests` (2 of 15 tests: `basicAutoConfiguration`, `honoursUsersEnableJdbcRepositoriesConfiguration`) |
| `module/spring-boot-data-ldap` | `org.springframework.boot.data.ldap.autoconfigure.DataLdapRepositoriesAutoConfigurationTests` (2 of 3 tests, added bin13 — same hypothesized upstream mechanism, different downstream exception shape) |
