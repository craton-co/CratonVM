# Hibernate entity-graph legacy-syntax parsing — ANTLR `RuleNode.getChildCount()` NullPointerException

| | |
|---|---|
| **Status** | ✅ FIXED — 2026-07-12. All 14 affected classes pass on the fixed CratonVM default-JIT build. |
| **Discovered** | 2026-07-11 full 4548-class suite audit, `dev` post `44f16ee2`+. |
| **Area** | ANTLR-based parsing of Hibernate's legacy entity-graph string syntax (`@NamedEntityGraph`/graph-parser DSL). |


## Resolution

The reported ANTLR RuleNode getChildCount NPE was a downstream symptom, not an
ANTLR parse-tree construction fault. Hibernate Models uses bound method
references such as Class::getDeclaredAnnotations and Class::getName while
building its annotation model. CratonVM method-reference dispatch selected
concrete real-JDK Class bytecode instead of the registered native accessor
required by its VM-side Class mirrors. As a result, getDeclaredAnnotations
returned an empty array and getName returned an internal slash-separated name;
Hibernate silently built zero entity bindings, causing the graph-parser failures.

invoke_on_class_shared_inner now forces the registered native overrides for
Class.getDeclaredAnnotations and Class.getName through this method-reference path.

## Validation

- HotSpot JDK 25: representative class passed.
- Fixed CratonVM: no-JIT method-reference probe reported real annotations and
  dot-form class names; direct SessionFactory probe built three entity bindings.
- Fixed CratonVM default-JIT rerun: all 14 listed classes passed, with no
  RuleNode or entity-binding residual.

## Symptom

Every affected class fails identically:
```
java.lang.NullPointerException: Cannot invoke "org.antlr.v4.runtime.tree.RuleNode.getChildCount()" because "node" is null
```
(3 of the 14 wrap it one level up: `java.lang.RuntimeException: Could not
build SessionFactory: Cannot invoke ... because "node" is null` — same root
NPE, hit earlier during SessionFactory bootstrap instead of at first query
time, depending on whether the class's graphs are parsed eagerly or lazily.)

## Affected classes (14)

```
entitygraph.EntityGraphFunctionalTests
entitygraph.FetchGenericAttributesTest
entitygraph.EntityGraphFetchingTest
entitygraph.parser.LegacySyntaxEntityGraphParserTest
entitygraph.parser.EntityGraphsTest
entitygraph.named.parsed.LegacySyntaxClassLevelTests
entitygraph.named.parsed.LegacySyntaxPackageLevelTests
graph.LegacySyntaxEntityGraphsTest
jpa.graphs.queryhint.QueryHintEntityGraphTest
jpa.graphs.FetchGraphTest
query.graph.QueryWithGraphTests
query.QueryHintTest
loading.semantics.EntityGraphLoadSemanticTests
fetching.GraphParsingTest
```

Every class name references "graph" parsing (entity graphs, fetch graphs,
`@NamedEntityGraph`) — a tight, coherent cluster centered on Hibernate's
ANTLR-based parser for the legacy string-form graph syntax (e.g.
`"name(attr1 attr2)"` fetch-graph definitions), not the annotation-based
form.

## Original root-cause hypothesis (superseded)

`org.antlr.v4.runtime.tree.RuleNode.getChildCount()` is called on a `null`
`node` reference somewhere in Hibernate's graph-parser visitor/listener code
that walks the ANTLR parse tree. This is either:
1. A genuine ANTLR parse-tree construction gap in CratonVM (a node that
   should have been populated during parsing is left `null` — possibly
   related to the same general ANTLR-interop area implicated in the
   separately-tracked wrong-object uncaught-exception-reporting bug, which
   also involved `org.antlr.v4.runtime.tree`-adjacent classes reported as
   "the exception" — see the spawned investigation for that bug), or
2. A visitor invoked on the wrong/empty tree (e.g. a `null`-returning parse
   call that should have thrown a syntax error instead of silently
   returning `null`, only surfacing as an NPE one level up when the caller
   assumes a non-null tree).

## Repro

Azure host harness, e.g.:
```bash
echo org.hibernate.orm.test.entitygraph.EntityGraphFunctionalTests > /tmp/one.txt
<cratonvm> --java-home <jdk25> --Xmx 1500m @common.linux.args -Dcraton.batch=1 CratonRunner /tmp/one.txt 0
```

## Original next steps (completed)

- Get a full stack trace (`CRATONVM_DBG_ATHROW=1`) to find exactly which
  Hibernate parser/visitor method calls `getChildCount()` on the null node,
  and trace backwards to where that node should have been set.
- Check whether this correlates with the separately-tracked wrong-object
  crash-reporting bug (`org/antlr/v4/runtime/CommonToken` reported as a
  fatal "exception" in an unrelated Hibernate HQL-parsing crash) — both
  involve ANTLR tree-node objects behaving unexpectedly, though the
  symptoms differ (a real NPE here vs. a wrong-exception-class report
  there); worth checking if they share a native ANTLR-support code path.
- Confirm CratonVM-specificity with a HotSpot run (pending).
