# `spring-boot-jdbc` + `spring-boot-mail` — a plain `java.naming.factory.initial`-installed `InitialContextFactory` is never consulted by CratonVM's synthetic `Context.lookup`

**Status: OPEN — found 2026-07-17 (confirms/root-causes an unconfirmed hypothesis filed the same day in a sibling doc, see below)**

## Symptom

9 test methods across 2 modules fail. Both use Spring Boot's own
`org.springframework.boot.autoconfigure.jndi.TestableInitialContextFactory`
test helper: `System.setProperty(Context.INITIAL_CONTEXT_FACTORY,
TestableInitialContextFactory.class.getName())` in `@BeforeEach`, then
`TestableInitialContextFactory.bind(name, object)` to seed a name before
exercising autoconfiguration that performs a JNDI `lookup(name)`.

| Module | Class | Failing tests |
|---|---|---|
| `module/spring-boot-jdbc` | `org.springframework.boot.jdbc.autoconfigure.JndiDataSourceAutoConfigurationTests` | 4 of 4 — whole class |
| `module/spring-boot-mail` | `org.springframework.boot.mail.autoconfigure.MailSenderAutoConfigurationTests` | 5 of 19 |

Representative trace (`spring-boot-jdbc`, name `"foo"` was bound via
`TestableInitialContextFactory.bind("foo", dataSource)` immediately before):

```
org.springframework.beans.factory.BeanCreationException: Error creating bean with name 'dataSource' ...: Failed to instantiate [javax.sql.DataSource]: Factory method 'dataSource' threw exception with message: Failed to look up JNDI DataSource with name 'foo'
     Caused by: org.springframework.jdbc.datasource.lookup.DataSourceLookupFailureException: Failed to look up JNDI DataSource with name 'foo'
       org.springframework.jdbc.datasource.lookup.JndiDataSourceLookup.getDataSource(JndiDataSourceLookup.java:48)
     Caused by: javax.naming.NameNotFoundException: NameNotFoundException: `java:jboss/exported/foo` not bound
```

Note the exception message: the lookup for plain name `"foo"` was silently
rewritten to `java:jboss/exported/foo` and failed against **that**
namespace — not against whatever `TestableInitialContextFactory` itself
bound `"foo"` to.

`spring-boot-mail`'s 5 failures are the same shape, either "context started
successfully but no `Session`/`JavaMailSenderImpl` bean was found" (the JNDI
lookup silently returned nothing usable) or "context should have failed to
start but didn't" (`jndiSessionNotAvailableWithJndiName` — the inverse
case, where the test expects a lookup-miss to properly fail startup, and
here it doesn't because it isn't running the code path it thinks it is).

Full logs:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard3/logs/module_spring-boot-jdbc.org.springframework.boot.jdbc.autoconfigure.JndiDataSourceAutoConfigurationTests.out.log`,
`.../shard4/logs/module_spring-boot-mail.org.springframework.boot.mail.autoconfigure.MailSenderAutoConfigurationTests.out.log`

## Root cause (CONFIRMED at file:line precision)

`native-builtins/src/wildfly_naming.rs` reimplements `InitialContext`/
`Context.lookup` entirely in Rust as a **flat, WildFly/Keycloak-style
in-memory namespace store** (`lookup_value`, backed by
`context_names_bind_info_for`), because WildFly's real naming subsystem
never runs stock JDK `InitialContext` bytecode. `do_context_lookup`
(`wildfly_naming.rs:951-1004`) only escapes that flat store for two cases:

1. A `NamingManager.hasInitialContextFactoryBuilder()`-style **builder** is
   installed (`builder_initial_context`, `wildfly_naming.rs:798-830`) — this
   checks `NamingManager.hasInitialContextFactoryBuilder()`, i.e. only code
   that called `NamingManager.setInitialContextFactoryBuilder(...)`.
2. The name uses the `java:` URL scheme (`is_java_url_scheme`), delegated to
   Tomcat's `org.apache.naming` URL-context-factory chain.

For any other (plain, relative) name, it falls straight to the flat
WildFly-style store: `lookup_value(name)` — which for a non-`java:` name
gets normalized to `java:jboss/exported/{name}` (`wildfly_naming.rs:245-246`,
`("jboss/exported", &["java","jboss","exported"])` at line 188) and looked
up there. `has_initial_context_provider` (`wildfly_naming.rs:627-649`) DOES
correctly check the `java.naming.factory.initial` **system property** — but
only to decide *which exception* to throw on a miss (a real
"`Need to specify class name in environment...`" `NoInitialContextException`
vs. a plain `NameNotFoundException`), never to actually **use** that
property to instantiate and delegate to the real `InitialContextFactory` it
names.

`TestableInitialContextFactory` (Spring Boot's standard JNDI test seam) uses
the **third**, most common JNDI SPI mechanism — a plain `InitialContextFactory`
class named via `java.naming.factory.initial` (not a `NamingManager` builder,
not a `java:` URL context) — which this routing simply never checks. Every
lookup for a plain name therefore always hits CratonVM's own internal
WildFly-namespace store, which has nothing under `java:jboss/exported/foo`
because the test bound `"foo"` into `TestableInitialContextFactory`'s own
(entirely separate, Java-side) map, never into CratonVM's Rust-side store.

**Confirmed by source reading** (this triage pass did not rebuild/run the
VM, but the code path is unambiguous: `do_context_lookup` has exactly two
escape hatches, both read in full, neither covers `java.naming.factory.initial`-named
plain factories).

## Relationship to a sibling same-day doc

`docs/known-issues/springboot/core-autoconfigure-singleton-fail-residuals-20260717.md`
(filed the same day, different module — `core/spring-boot-autoconfigure`'s
`ConditionalOnJndiTests`) independently observed the same class of failure
and filed it as an **unconfirmed hypothesis**: "these two tests install a
mock/test `InitialContextFactory` (via `TestableInitialContextFactory` or
similar) ... On CratonVM the condition evaluates as if JNDI is unavailable
... most likely in `InitialContext` construction/lookup ... Not traced
further." This doc's root-cause analysis (above) confirms and pins that
exact hypothesis at `native-builtins/src/wildfly_naming.rs:951-1004` /
`798-830` / `627-649`. Not editing that sibling doc (different module, out
of this triage batch's scope) — noting the connection here so the two can
be linked/merged by whoever consolidates the index.

## Affected classes

| Module | Class | Failing tests |
|---|---|---|
| `module/spring-boot-jdbc` | `org.springframework.boot.jdbc.autoconfigure.JndiDataSourceAutoConfigurationTests` | 4 of 4 |
| `module/spring-boot-mail` | `org.springframework.boot.mail.autoconfigure.MailSenderAutoConfigurationTests` | 5 of 19 |
