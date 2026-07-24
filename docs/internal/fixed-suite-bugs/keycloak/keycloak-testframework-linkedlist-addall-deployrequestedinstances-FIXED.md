# Keycloak test-framework `deployRequestedInstances` resolution failure — FIXED

Status: FIXED (branch `fix/kc-registry-deployrequested-instances-20260706`)

Date observed: 2026-07-04. Root-caused and fixed: 2026-07-06.

## Original symptom

`tests/base :: org.keycloak.tests.admin.identityprovider.IdentityProviderMapperTest`
failed all 5 test methods in `beforeEach` with:

```
java.lang.RuntimeException: Failed to resolve next requested instance to deploy
    org.keycloak.testframework.injection.Registry.lambda$deployRequestedInstances$1(Registry.java:248)
    org.keycloak.testframework.injection.Registry.deployRequestedInstances(Registry.java:248)
    org.keycloak.testframework.injection.Registry.beforeEach(Registry.java:132)
    org.keycloak.testframework.KeycloakIntegrationTestExtension.beforeEach(KeycloakIntegrationTestExtension.java:30)
```

This is Keycloak test-framework's dependency-injection `Registry`, doing a
repeated-selection topological sort over `requestedInstances`: each pass picks
the first entry whose declared `Dependency` list is fully satisfied by
`deployedInstances`; if no entry qualifies, it throws the exception above.

## Root cause

**Not a bug in the Keycloak test-framework or its dependency-resolution
algorithm.** `Registry.findRequestedInstances()` calls
`DependencyGraphResolver` to compute any *missing* dependency instances (e.g.
`ManagedCertificates`/`CertificatesSupplier`, `TestDatabase`/
`DevMemDatabaseSupplier`, needed transitively by `DistributionKeycloakServerSupplier`),
then does:

```java
List<RequestedInstance<?, ?>> missingInstances = dependencyGraphResolver.getMissingInstances();
requestedInstances.addAll(missingInstances);          // Registry.java:204
```

Both `requestedInstances` and `missingInstances` are plain `java.util.LinkedList`.
**`LinkedList.addAll(Collection)` silently added nothing** when the argument was
itself a `LinkedList` — confirmed with a minimal standalone repro:

```java
List<String> a = new LinkedList<>();      // 7 elements
List<String> b = new LinkedList<>();      // 8 elements
a.addAll(b);                              // CratonVM: returns false, a.size() stays 7
                                           // HotSpot:  returns true,  a.size() becomes 15
```

`LinkedList.addAll(ArrayList)`, `ArrayList.addAll(LinkedList)`,
`linkedList.toArray()`, and manual iterator `add()` all worked correctly — only
`LinkedList.addAll(LinkedList)` (both sides native/synthetic `LinkedList`) was
broken. Because the 8 missing instances (Certificates, TestDatabase,
KeycloakUrls, AdminClient, etc.) never actually joined `requestedInstances`,
`DistributionKeycloakServerSupplier`'s declared dependencies on
`ManagedCertificates`/`TestDatabase` could never be satisfied by anything in
`deployedInstances`, so every pass of `deployRequestedInstances()`'s
topological sort found nothing resolvable and threw.

### Where the bug actually lived

`java.util.LinkedList` is one of CratonVM's native/synthetic collection
classes: its real head/tail/size live in an identity-hash-keyed side-table
overlay (`ll_overlay()`), not in the object's raw field slots — see the
`ll_get`/`ll_set` block comment in `../../../../native-collections/src/lib.rs` (the
synthetic slot indices alias real-JDK declared fields once the real
`java.util.LinkedList` class is loaded).

`native_ll_add_all` (registered for `java/util/LinkedList.addAll(Ljava/util/Collection;)Z`)
delegates to a shared helper, `collect_collection_elements`, that sniffs the
*argument* collection's runtime shape to pull out its elements generically
(it also backs `ArrayList.addAll`, copy constructors, etc.). Its LinkedList
branch read the argument's size/head via **raw field slots**:

```rust
if LL_FIELD_SIZE < n_fields {
    if let Value::Int(size) = ctx.get_field(coll, LL_FIELD_SIZE) {   // WRONG: bypasses the overlay
        ...
```

Since the real state lives in the overlay, this raw read saw garbage/zero,
`size > 0` never matched, and the function fell through every remaining
heuristic to `Vec::new()` — so `addAll` (and any other caller of
`collect_collection_elements` fed a `LinkedList` argument) silently added
nothing.

## Fix

`../../../../native-collections/src/lib.rs`, `collect_collection_elements`: detect a real
`LinkedList` (or subclass) argument explicitly (`class_id_by_name` +
`is_subclass`, the same pattern already used for the HashSet/TreeSet branches
just below it) and read its size/head through the overlay-aware `ll_get()` —
the same accessor `ll_snapshot_array` and every other native LinkedList method
already use — instead of raw `ctx.get_field`.

Verified:
- Minimal repro above now returns `true`/size 15 on CratonVM, matching HotSpot.
- `cargo test -p cratonvm-native-collections`: 14+4+3 existing tests still pass, no regressions.
- `IdentityProviderMapperTest` no longer throws "Failed to resolve next
  requested instance to deploy" — `deployRequestedInstances()` now correctly
  proceeds through `DistributionKeycloakServer.start()` (the dependency graph
  resolves as it does under real HotSpot).

## Residual — do NOT expect this test to fully PASS yet

Getting past this bug immediately exposes a **separate, new** hang: booting
the `DistributionKeycloakServer` calls into SmallRye's Sisu bean-loading
(`io.smallrye.beanbag.sisu.BeanLoadingTaskRunner.waitForCompletion()`), which
blocks forever on `java.util.concurrent.Phaser.arriveAndAwaitAdvance()` inside
`ForkJoinPool.managedBlock`/`unmanagedBlock`. Stack-dumped via
`--stack-dump-on-timeout 90`; only one thread (`tid=0`) is captured blocked
there, suggesting whatever background worker(s) should call `Phaser.arrive()`
to release it never run to completion (or never get scheduled) under
CratonVM. This is tracked separately:
[keycloak-testframework-phaser-forkjoinpool-sisu-hang.md](../../known-issues/keycloak-07-04/keycloak-testframework-phaser-forkjoinpool-sisu-hang.md).

## Evidence

- Minimal repro: `LLAddAllRepro.java` / `LLAddAllRepro2.java` (isolated
  `LinkedList.addAll` before/after CratonVM vs HotSpot).
- Full-class stack dump showing the Phaser hang after the fix:
  `cratonvm-fixed-stackdump.log` (scratch, not committed).
- Fix branch: `fix/kc-registry-deployrequested-instances-20260706`.
