# `DataJdbcTestIntegrationTests`: Spring Data JDBC's metamodel treats a plain `String` field as an unresolvable association

**Status: FIXED — retired 2026-07-18. The original investigation below is retained for diagnostic history.**

## Resolution (2026-07-18)

**Status: FIXED / record retired.** The optional jMolecules lookup was already
correct and is retained as an independent class-loading regression. The actual
failure was lambda dispatch: `LambdaMetafactory` supplied a private synthetic
body as an `InvokeVirtual` handle, and CratonVM re-resolved that handle against
the captured property's concrete subclass. Spring Data subclasses contain
same-named `lambda$new$N` methods, so the VM executed the subclass supplier
instead of `AbstractPersistentProperty.lambda$new$2`.

Private lambda implementation handles now dispatch to their resolved declaring
class in both the interpreter and native-context lambda paths. Ordinary virtual
method references keep receiver-based dispatch. `RPrivateLambdaOwner` covers a
parent/child synthetic-lambda-name collision; `ROptionalClassForName` continues
to verify absent optional dependencies.

Validation: the real Spring Data JDBC metamodel reports `association=false`
for `Long` and `String` fields with JIT enabled and with `--nojit`, followed by
the real `DataJdbcTestIntegrationTests` runner class in both modes.

## Original report

## Symptom

| Class | tests failed/total |
|---|---:|
| `DataJdbcTestIntegrationTests` | 1/5 |

```
JUnit Jupiter:DataJdbcTestIntegrationTests:testRepository()
    => java.lang.IllegalArgumentException: Cannot determine reference type type for private final java.lang.String org.springframework.boot.data.jdbc.test.autoconfigure.ExampleEntity.name
       org.springframework.data.jdbc.core.convert.Association.from(Association.java:74)
       org.springframework.data.jdbc.core.convert.SqlGenerator$Columns.lambda$populateColumnNameCache$0(SqlGenerator.java:1407)
       org.springframework.data.mapping.PersistentEntity.lambda$doWithAll$0(PersistentEntity.java:294)
       org.springframework.data.mapping.model.BasicPersistentEntity.doWithAssociations(BasicPersistentEntity.java:375)
       org.springframework.data.jdbc.core.convert.SqlGenerator$Columns.populateColumnNameCache(SqlGenerator.java:1403)
       org.springframework.data.jdbc.core.convert.SqlGenerator$Columns.<init>(SqlGenerator.java:1384)
       org.springframework.data.jdbc.core.convert.SqlGenerator.<init>(SqlGenerator.java:130)
       org.springframework.data.jdbc.core.convert.SqlGeneratorSource.lambda$getSqlGenerator$0(SqlGeneratorSource.java:71)
       org.springframework.data.jdbc.core.convert.DefaultDataAccessStrategy.findAll(DefaultDataAccessStrategy.java:306)
       org.springframework.data.jdbc.repository.support.SimpleJdbcRepository.findAll(SimpleJdbcRepository.java:95)
       org.springframework.boot.data.jdbc.test.autoconfigure.DataJdbcTestIntegrationTests.testRepository(DataJdbcTestIntegrationTests.java:62)
```

Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/module_spring-boot-data-jdbc-test.org.springframework.boot.data.jdbc.test.autoconfigure.DataJd-89616a520263.out.log`

The entity in question (this worktree's own test fixture,
`ExampleEntity.java`) is entirely ordinary:

```java
@Table("EXAMPLE_ENTITY")
public class ExampleEntity {
	@Id
	private Long id;
	private final String name;
	private final String reference;
	public ExampleEntity(String name, String reference) { ... }
	...
}
```

`name` is a plain `private final String` field with no annotations beyond
what's on the class — nothing that should make Spring Data JDBC treat it
as a reference to another aggregate root.

## Historical hypothesis (superseded)

The private-final-field/reflection hypothesis below was disproved. Direct
`Field.getType()` and `Field.getGenericType()` checks were correct in both
execution modes; the residual was the private-lambda owner dispatch described
above.

Spring Data JDBC's `BasicPersistentEntity.doWithAssociations` walks every
persistent property Spring Data's metamodel does **not** classify as a
"simple" scalar type, treating each as a potential `Association` to
another entity; `Association.from()` throws exactly this
`IllegalArgumentException` when it cannot resolve what the referenced
type actually is. Real HotSpot never reaches this path for `name` — `String`
is a hard-coded member of Spring Data's `SimpleTypeHolder` and should
short-circuit out of the association walk entirely before
`Association.from()` is ever called on it. That CratonVM instead runs
`name` through the association-resolution path at all suggests Spring
Data's classification step — which relies on reflectively inspecting the
field/property's declared type (`Field.getType()`/`PropertyDescriptor`-style
introspection via `BasicPersistentEntity`'s `Property` wrapper) — is not
correctly reporting `String` for this field, or is failing to short-circuit
for some other CratonVM-specific reason. This has not been narrowed further
this session (no check of `native-builtins/src/lang_class.rs`'s
`Field`/`getGenericType` path, and no comparison against whether the
sibling `reference` field — also `private final String`, immediately after
`name` in declaration order — would hit the identical error if `name`'s
exception didn't abort the walk first). Given the codebase's own
documented history of subtle field-layout/reflection gaps specifically
narrow to certain field-modifier combinations (`private final`, in
particular, has been the trigger for distinct bugs elsewhere in this
codebase per project history), a `private final` field-specific reflection
gap is a plausible category for this, but unconfirmed.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-data-jdbc-test` | `org.springframework.boot.data.jdbc.test.autoconfigure.DataJdbcTestIntegrationTests` |
