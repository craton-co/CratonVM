# Bug 10 — `NoClassDefFoundError: org/apache/kafka/clients/MetadataSnapshot`

## RESOLVED (2026-06-12) — NOT a CratonVM bug: harness classpath version skew

The original diagnosis below ("masked `<clinit>` failure", "the class IS on the
classpath") was **wrong**. `org.apache.kafka.clients.MetadataSnapshot` does **not
exist** in `kafka-clients-3.7.0.jar` (that version still has the old
`MetadataCache`; the rename landed in **3.7.2**). The test classes in
`apps/kafka-test-classes` are built against **kafka 3.7.2**, but
`apps/kafka/tests/cp.txt` pinned the runtime `kafka-clients` to **3.7.0** — a
version skew. The class is genuinely absent, so every reference throws
`ClassNotFoundException` → surfaced later as `NoClassDefFoundError`.

**Proof it is not CratonVM-specific:** HotSpot 25 with the same 3.7.0 cp ALSO
throws `ClassNotFoundException: org.apache.kafka.clients.MetadataSnapshot`. The
comparison harness should have excluded it (HotSpot-also-fails), so it was never
a real CratonVM-only defect — the "46 failures" were all this skew.

**Fix:** `cp.txt` now points `kafka-clients` at `3.7.2` (the version the test
classes were compiled against). With that jar:
`MetadataSnapshotTest` → **5/5 on CratonVM and 5/5 on HotSpot.** No VM change.

---

## (original, incorrect diagnosis kept for the record)

Original title: `NoClassDefFoundError: org/apache/kafka/clients/MetadataSnapshot` (masked `<clinit>` failure)

**Severity:** High — 46 failures; breaks `MetadataSnapshotTest` (all 5),
`MetadataTest`, `producer.internals.RecordAccumulatorTest`. The class IS on the
classpath, so this is a **masked static-initializer failure**, not a missing jar.
Reproduces under `--nojit`. HotSpot clean.

## Symptom
```
=> java.lang.NoClassDefFoundError: org/apache/kafka/clients/MetadataSnapshot
   org.apache.kafka.clients.MetadataSnapshotTest.testTopicNamesCacheBuiltFromTopicIds(MetadataSnapshotTest.java:159)
```
`org.apache.kafka.clients.MetadataSnapshot` exists in `kafka-clients-3.7.0.jar`.
A `NoClassDefFoundError` (rather than `ClassNotFoundException`) means a **prior**
initialization of the class failed — the first touch threw an
`ExceptionInInitializerError` from `MetadataSnapshot.<clinit>` (or a superclass /
referenced class init), leaving the class in the *erroneous* state; every later
reference then surfaces `NoClassDefFoundError`.

## Root cause (to pin down)
`MetadataSnapshot.<clinit>` (or a class it statically references) throws on
CratonVM. CratonVM's unhandled-exception renderer does not show the original
`ExceptionInInitializerError` cause here (limited heap-side stack traces), so the
masked cause must be recovered. Suggested approach:
- Run a minimal repro that just does `Class.forName("org.apache.kafka.clients.MetadataSnapshot")`
  under CratonVM and capture the *first* error (should be `ExceptionInInitializerError`
  with the real cause) — e.g. with `CRATONVM_DBG_NSME`/clinit tracing.
- `MetadataSnapshot` holds static fields and an `EMPTY_SNAPSHOT` constant built from
  `Collections.emptyMap()`/`Cluster.empty()` etc.; the `<clinit>` likely trips an
  existing CratonVM intrinsic (collection/Cluster construction) during class init.

## Repro
```
cratonvm --nojit -cp <suite-cp> KRun org.apache.kafka.clients.MetadataSnapshotTest
# or a 3-line main: Class.forName("org.apache.kafka.clients.MetadataSnapshot")
```

## Affected classes (partial — append more later)
- clients.MetadataSnapshotTest (5/5 fail)
- clients.MetadataTest (partial)
- producer.internals.RecordAccumulatorTest
