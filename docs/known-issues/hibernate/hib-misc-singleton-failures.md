# Hibernate suite — remaining small/single-occurrence failure clusters

| | |
|---|---|
| **Status** | 🔴 OPEN (multiple independent small clusters, grouped here for coverage — see each sub-section). HotSpot confirmation pending for all. |
| **Discovered** | 2026-07-11 full 4548-class suite audit, `dev` post `44f16ee2`+. |

Catch-all doc for distinct exception types in the 453-class non-passed list
that occurred too few times to warrant their own doc, grouped by shared
exception type/theme. The four serialization EOF residuals were fixed and
retired with the related connection/proxy cluster; see
[`hib-connections-proxy-serializationexception-cluster-FIXED.md`](../../internal/fixed-suite-bugs/hib-connections-proxy-serializationexception-cluster-FIXED.md).

## InvalidMappingException — hbm/orm XML mapping-document parse failures (3 classes)

```
extendshbm.ExtendsTest                            :: Could not parse mapping document: .../packageentitynames.hbm.xml (RESOURCE)
bootstrap.binding.annotations.access.xml.XmlAccessTest :: Could not parse mapping document: .../Tourist2.xml (RESOURCE)
jpa.xml.versions.JpaXsdVersionsTest                :: Could not parse mapping document: .../valid-orm-1_0.xml (RESOURCE)
```
Three different mapping files, two different mapping formats (legacy
`.hbm.xml` and JPA `orm.xml`) — Hibernate's `InvalidMappingException` wraps a
lower-level parse failure (likely a SAX/DOM XML parse error or XSD/DTD
validation failure) that isn't visible in the truncated message. Given
`JpaXsdVersionsTest` specifically tests XSD-version compatibility, this
looks like it could be an XML-parser or XSD-validation gap in CratonVM's
real-JDK-mode `javax.xml`/`jakarta.xml` support — possibly related to the
QName loader-identity issue in
[hib-boot-models-xml-qname-jandex-cluster.md](hib-boot-models-xml-qname-jandex-cluster.md).

## SyntaxException — HQL `not (...)` boolean-negation parse failure (2 classes)

```
query.hql.LiteralTests        :: SyntaxException: At 1:0 and token 'from', mismatched input 'from' expecting WIT [from EntityOfBasics e1 ...]
mapping.basic.BooleanMappingTests :: SyntaxException: At 1:0 and token 'from', mismatched input 'from' expecting WIT [from EntityOfBooleans where not (convertedYesNo)]
```
Both queries involve boolean literal/converted-boolean handling in a `not
(...)` HQL clause. "expecting WIT" is truncated (likely "WITH" — a CTE
keyword) — suggests the ANTLR grammar's error recovery is reporting an
unexpected expected-token set, which could mean the query itself is
mis-tokenized earlier (a lexer-level issue feeding a bogus token stream to
the parser) rather than a genuine grammar gap. Possibly related to the
broader ANTLR-adjacent issues in this audit
([hib-entitygraph-antlr-rulenode-npe-cluster.md](hib-entitygraph-antlr-rulenode-npe-cluster.md)),
though the symptom (a real `SyntaxException`, not an NPE) differs.

## SQLGrammarException — two independent causes (2 classes)

```
delegation.SessionDelegatorBaseImplTest :: Error executing work [Syntax error ... "An exception has occurred in the compiler (25.0.3). Please fil[e a bug report]..."]
sql.storedproc.StoredProcedureResultSetMappingTest :: Could not prepare statement [Function "ALLEMPLOYEENAMES" not found; ...]
```
- `SessionDelegatorBaseImplTest`'s failure is H2's **in-process `javac`
  compiler** reporting an internal compiler crash ("An exception has
  occurred in the compiler") — this class and this exact failure mode was
  previously root-caused and fixed as **HIB-CV-27** (a `File.pathSeparator`
  post-clinit gap corrupting javac's classpath decoding — see
  [hib-linux-fail-bucket-triage-20260703.md](../hib-linux-fail-bucket-triage-20260703.md)
  item B). Its reappearance here is either a **regression** of that fix, or
  a **different** in-process-javac failure mode (the message text differs —
  the original was "package org.h2.tools does not exist"; this one is a
  compiler internal exception). Needs a fresh look to determine which.
- `StoredProcedureResultSetMappingTest`'s "Function ALLEMPLOYEENAMES not
  found" is a distinct, unrelated issue — a user-defined SQL function
  registered via H2's dynamic Java-function mechanism isn't being found at
  call time, possibly the same in-process-javac family (if the function's
  Java source failed to compile/register silently) or a separate H2
  function-registration gap.

## UnknownNamedQueryException / CannotContainSubGraphException (1 class each)

```
query.named.Jpa4StaticQueryRegistrationTest :: UnknownNamedQueryException: No query named 'org.hibernate.orm.test.query.named.Book#findByTitle()'
entitygraph.parser.EntityGraphParserTest     :: CannotContainSubGraphException: Attribute '...GraphParsingTestEntity.name' is of type 'java.lang.Stri[ng...]'
```
Two unrelated one-off gaps: a static (annotation-declared) named-query
registration not taking effect, and an entity-graph-parser validation
throwing where it apparently shouldn't (attaching a sub-graph to what it
thinks is a non-associable `String` attribute) — the latter is in the same
`entitygraph.parser` package as the ANTLR NPE cluster but a different
symptom (a real, specific validation exception vs. a null-node crash) — may
or may not share a root cause; not established.

## `FailureExpectedExtension$ExpectedFailureDidNotFail` — NOT a CratonVM defect (3 classes)

```
collection.set.PersistentSetTest#testCompositeElement...
collection.set.PersistentSetNonLazyTest#testComposit...
onetoone.cache.OneToOneCacheTest#OneToOneCacheByFore...
```
These are Hibernate's own test suite marking specific test methods
`@FailureExpected` (a known, tracked upstream Hibernate bug the test suite
itself doesn't expect to be fixed). CratonVM's run does NOT reproduce the
expected failure — i.e., **the test passes when Hibernate's own suite says
it should fail**. This is the *opposite* of a CratonVM bug (CratonVM
behaving more correctly than even HotSpot+real-Hibernate does in these
specific known-buggy-upstream cases) — flagged here for completeness per
the audit's "document every non-passing class" scope, but this is a test-
framework expectation mismatch, not a defect to fix. No action needed.
