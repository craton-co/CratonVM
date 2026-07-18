# `spring-boot-data-elasticsearch`: loopback connect-refused surfaces as timeout; Spring Data association-mapping always throws for a City field

**Status: RESOLVED 2026-07-18. Two unrelated failures, one per class.**

## Resolution

The association failure was not an optional-class-loading error. The
interpreter's virtual lambda dispatch resolved a private synthetic
`lambda$new$2` against a subclass with the same generated name and
descriptor, rather than against its declaring class. Private lambda targets
now retain their exact implementation owner; Spring Data's absent jMolecules
association type is consequently observed as null and ordinary Elasticsearch
document fields are no longer treated as associations.

The health timeout combined two Windows NIO defects: a closed loopback port
could remain pending after a raw non-blocking connect, and the resulting
failure did not reach the Apache reactor. A pre-connect loopback probe now
classifies a refused connection, preserves it as a native terminal state, and
surfaces it at the reactor's first I/O boundary. Socket-channel and plain
socket bridges preserve `ConnectException`/`SocketTimeoutException`; refused
connect messages are canonicalized as `Connection refused` rather than using
localized Winsock text.

Validation with the isolated `cratonvm-sb-data-es-connectassoc-20260718.exe`:

* `DataElasticsearchReactiveHealthIndicatorTests` PASS with JIT on and off.
* `DataElasticsearchAutoConfigurationTests` PASS with JIT on and off.
* `nb_connect::imp_windows::tests::closed_loopback_port_reports_connection_refused` PASS.

## Case 1 — `DataElasticsearchReactiveHealthIndicatorTests.elasticsearchIsDown()`: closed-port connect surfaces as `ConnectTimeoutException`, not "Connection refused"

```
=> java.lang.AssertionError:
Expecting actual:
  "org.apache.hc.client5.http.ConnectTimeoutException: Connect to http://localhost:65088 [localhost/0:0:0:0:0:0:0:1, localhost/127.0.0.1] failed: 1000 MILLISECONDS"
to contain:
  "Connection refused"
       org.springframework.boot.data.elasticsearch.health.DataElasticsearchReactiveHealthIndicatorTests.elasticsearchIsDown(DataElasticsearchReactiveHealthIndicatorTests.java:98)
```

Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/module_spring-boot-data-elasticsearch.org.springframework.boot.data.elasticsearch.health.DataE-5bb7f2032047.out.log`

The test connects to `localhost:65088` (a port nothing is listening on) and
expects the immediate OS-level `ECONNREFUSED`/RST that a real HotSpot
process gets when connecting to a closed loopback port — Apache
HttpComponents 5 surfaces that as a message containing "Connection
refused". Instead, the connection attempt runs for the full configured
1000ms client timeout and then fails with `ConnectTimeoutException`,
meaning **CratonVM's TCP connect to a closed local port does not receive an
immediate refusal** the way HotSpot's does; the caller only finds out via
its own timeout.

**Root cause — hypothesis, moderately confident (grounded in source this
session).** The suite runner sets `CRATONVM_REAL_NET_SOCKETS=1`
(`apps/spring-boot-suite-runner/run-spring-boot-suite.ps1:706`), routing
this connect through `native-io/src/socket_channel.rs`'s non-blocking
connect path and its Windows poll helper,
`native-io/src/nb_connect.rs` (`imp_windows::poll`, ~lines 176-217). That
function issues `WSAPoll` watching only `WSAPOLLWRNORM` (writable), then
only inspects `SO_ERROR` inside the branch reached when `WSAPoll` actually
reports an event (`n > 0`). This is a well-known Windows `WSAPoll`
limitation: unlike Unix `poll()`, Windows `WSAPoll` does not reliably
surface a failed/refused connect as a `WSAPOLLERR`/`WSAPOLLHUP` event the
way it surfaces a successful one as `WSAPOLLWRNORM` — Windows historically
requires `select()`'s `exceptfds` (or a different completion notification
mechanism) to detect a refused connect promptly. If `WSAPoll` returns `n ==
0` (no event) for a connection that the OS has actually already refused,
this code returns `ConnectPoll::Pending` and the caller only finds out via
its own configured timeout — matching the observed 1000ms-then-`ConnectTimeoutException`
symptom exactly. Not fully bisected to a live repro this session; a
standalone `SocketChannel.connect()` to a known-closed loopback port,
compared against real HotSpot's near-instant refusal, would confirm this
quickly.

A secondary, independently real (but not proven to fully explain this
symptom) gap: `native-builtins/src/plain_socket.rs::socket_connect`
(~lines 311-374) throws a bare generic `IOException` for every connect
failure rather than distinguishing `ConnectException`/`SocketTimeoutException`
— worth checking as part of any fix, since even a correctly-detected
refusal needs to surface as the right exception type/message for
HttpComponents 5's "Connection refused" text match to pass.

There is a related-but-distinct, already-filed HttpComponents5/CratonVM
async-connect gap in the same `native-io` reactor family — see
`docs/known-issues/springboot/README.md`'s
`spring-web-flow-outputstreamwriter-close-corruption.md` root cause #3
(connect *succeeds* but data never flows) — not confirmed to share this
bug's exact mechanism, but worth a joint look given both live in the
HttpClient5 IOReactor/native-io path.

## Case 2 — `DataElasticsearchAutoConfigurationTests.shouldFilterInitialEntityScanWithDocumentAnnotation()`: `City` entity's association mapping always throws `UnsupportedOperationException`

```
=> java.lang.IllegalStateException: Unstarted application context ... failed to start
 Caused by: org.springframework.beans.factory.BeanCreationException: Error creating bean with name 'elasticsearchMappingContext' ...: Cannot create PersistentEntity for 'org.springframework.boot.data.elasticsearch.domain.city.City'
 Caused by: org.springframework.data.mapping.MappingException: Cannot create PersistentEntity for 'org.springframework.boot.data.elasticsearch.domain.city.City'
 Caused by: java.lang.UnsupportedOperationException
       org.springframework.data.elasticsearch.core.mapping.SimpleElasticsearchPersistentProperty.createAssociation(SimpleElasticsearchPersistentProperty.java:365)
       org.springframework.data.mapping.model.AbstractPersistentProperty.lambda$new$0(AbstractPersistentProperty.java:89)
       org.springframework.data.mapping.model.AbstractPersistentProperty.getAssociation(AbstractPersistentProperty.java:217)
       org.springframework.data.mapping.PersistentProperty.getRequiredAssociation(PersistentProperty.java:226)
       org.springframework.data.mapping.context.AbstractMappingContext$PersistentPropertyCreator.createAndRegisterProperty(AbstractMappingContext.java:660)
       org.springframework.util.ReflectionUtils.doWithFields(ReflectionUtils.java:727)
       org.springframework.data.mapping.context.AbstractMappingContext.doAddPersistentEntity(AbstractMappingContext.java:474)
```

Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/module_spring-boot-data-elasticsearch.org.springframework.boot.data.elasticsearch.autoconfigur-940eb166cb1e.out.log`
(1 of 8 tests fails this way)

`SimpleElasticsearchPersistentProperty.createAssociation()` is real Spring
Data Elasticsearch source that **unconditionally** throws
`UnsupportedOperationException` — Spring Data Elasticsearch does not support
relational-style "associations" at all. It is only reachable through
`PersistentProperty.getRequiredAssociation()`, which is itself only called
when `AbstractMappingContext`'s per-field introspection
(`ReflectionUtils.doWithFields` over `City`'s declared fields) has already
concluded that some particular field on
`org.springframework.boot.data.elasticsearch.domain.city.City` **is** an
association (Spring Data's `Property.isAssociation()`, driven by generic
type/annotation reflection on the field). On real HotSpot, no field of
`City` gets classified this way, so the mapping context builds cleanly.

**Mechanism confirmed via `javap` disassembly of the real jars; the exact
CratonVM defect is still unconfirmed.** `SimpleElasticsearchPersistentProperty.createAssociation()`
is confirmed (via `javap` on the real `spring-data-elasticsearch-6.1.0-RC1.jar`)
to unconditionally `throw new UnsupportedOperationException()` — an
intentional "must never be reached" stub, since Elasticsearch documents
don't support associations. It's only reached when `isAssociation()`
returns `true`, inherited unchanged from `AbstractPersistentProperty`
(`spring-data-commons-4.1.0-RC1`), whose real logic (via `javap`
disassembly of `lambda$new$2`) is:
`ASSOCIATION_TYPE != null && ASSOCIATION_TYPE.isAssignableFrom(rawType)`,
where `ASSOCIATION_TYPE = ClassUtils.loadIfPresent("org.jmolecules.ddd.types.Association", loader)`.
`City`'s actual fields
(`apps/spring-boot/module/spring-boot-data-elasticsearch/.../domain/city/City.java`)
are plain `Long id`/`String name,state,country,map` — nothing
jMolecules-related — and `org.jmolecules.ddd.types.Association` is
genuinely absent from the classpath (only a `jmolecules-bom` POM exists in
the Gradle cache, no actual jar). On correct behavior, `Class.forName` for
that name must throw `ClassNotFoundException`, `ASSOCIATION_TYPE` stays
`null`, and `isAssociation()` is always `false`.

**Exact CratonVM defect — still a hypothesis, unconfirmed.** Something in
CratonVM's class resolution is letting `ASSOCIATION_TYPE` end up non-null
(or otherwise routing execution into `createAssociation()`) for this
genuinely-absent class. The two most likely known CratonVM mechanisms for
"probe for an absent class silently resolves instead of throwing
`ClassNotFoundException`" were checked and **ruled out**:
`is_enterprise_stub_prefix` (`classloading/src/class_manager.rs:7054-7063`,
the WildFly/Quarkus/etc. synthetic-stub fallback) does not list
`org/jmolecules/`; and the already-fixed `BUG-G`
(`docs/internal/CRATONVM_BUGS/BUG-G-classforname-never-throws-cnfe.md`)
only scoped its fix (and the still-live stub-fallback gate it left in
place, `is_jdk_class`, `class_manager.rs:7065-7080`) to
`java/javax/sun/jdk/com.sun` prefixes — `org/jmolecules/` isn't in that set
either, so this isn't literally the same code path, though it is the same
*family* of bug (a third-party "does this optional class exist" probe
misresolving on CratonVM). Pinning the exact mechanism needs live debugging
(e.g. `CRATONVM_S111_DBG=1` around `native_class_for_name`), not attempted
this session.

**Related, not confirmed identical:** a same-day sibling doc,
`docs/known-issues/springboot/datajdbctestintegrationtests-association-from-reference-type-npe.md`,
reports a superficially similar symptom — a plain `String` field wrongly
routed through Spring Data JDBC's association-resolution walk — but via a
mechanically distinct path (`SimpleTypeHolder`/`BasicPersistentEntity.doWithAssociations`
classification, not this bug's `ASSOCIATION_TYPE`/jMolecules-presence
check). Likely a different bug sharing the same "Spring Data metamodel
misjudges class presence/simple-type-ness" symptom family — worth a joint
follow-up, not proven identical.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-data-elasticsearch` | `org.springframework.boot.data.elasticsearch.health.DataElasticsearchReactiveHealthIndicatorTests` (Case 1, 1 of 5 tests) |
| `module/spring-boot-data-elasticsearch` | `org.springframework.boot.data.elasticsearch.autoconfigure.DataElasticsearchAutoConfigurationTests` (Case 2, 1 of 8 tests) |
