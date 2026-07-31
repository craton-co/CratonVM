# WildFly `WFLYCTL0079`: duplicate transaction attribute registration

Status: **CLOSED AS NOT REPRODUCIBLE, RETIRED 2026-07-31.**

Be precise about what that means: **the producer was never caught in the act.**
What changed is that the failing statement is now identified exactly — and
confirmed by a negative control that reproduces the historical message
byte-for-byte — that the previous "next discriminator" in this report was aimed
at code which cannot produce the failure, that six genuine GC-rooting defects on
that statement's CratonVM path were found and fixed, and that a campaign worth
roughly 2.9 million executions of the suspect sequence produced zero
occurrences against a historical rate near 1 in 1,400. This is a
not-reproducible closure, not a demonstrated fix. The harness is committed at
`docs/known-issues/repros/wfly0079-dup-attr/` and the reopening recipe at the
bottom names exactly which of the two remaining producers a fresh occurrence
would point at.

## Symptom

Very rarely, WildFly 32 parallel extension boot aborted while installing the
transactions subsystem:

```text
java.lang.RuntimeException: WFLYCTL0079: Failed initializing module org.jboss.as.transactions
Caused by: java.lang.IllegalArgumentException: WFLYCTL0043: An attribute named
'hornetq-store-enable-async-io' is already registered at location '/subsystem=transactions'
```

Historical campaigns observed two matching failures across roughly 4,800 boots;
the retired stale-reference report estimated ~1 in 1,200–1,600. Neither
occurrence carried the stale-receiver, pointer-map, zeroed-from-space, or
checkcast evidence of the GC family in
`wildfly-interpreter-operand-stack-slot-stale-after-nested-alloc-FIXED.md`.

## What actually produces this message

`WFLYCTL0043` is thrown by
`org.jboss.as.controller.registry.ConcreteResourceRegistration.storeAttribute`:

```java
private void storeAttribute(AttributeDefinition def, AttributeAccess aa) {
    String name = def.getName();
    writeLock.lock();
    if (attributes.containsKey(name)) throw alreadyRegistered("attribute", name);
    attributes.put(name, aa);
    ...
}
```

`attributes` is a plain `HashMap<String, AttributeAccess>`. So the message
requires the same name string to reach `storeAttribute` twice for one
registration, or `containsKey` to answer positively for a name stored once.

`TransactionSubsystemRootResourceDefinition.registerAttributes` is the only
caller for `/subsystem=transactions`, and its shape is decisive:

```java
Set<AttributeDefinition> attributesWithoutMutuals = new HashSet<>(Arrays.asList(add_attributes));
attributesWithoutMutuals.remove(USE_HORNETQ_STORE_PARAM);
...                                                              // 12 removes in total
attributesWithoutMutuals.remove(HORNETQ_STORE_ENABLE_ASYNC_IO);  // <-- the one that matters

for (final AttributeDefinition def : attributesWithoutMutuals) {
    resourceRegistration.registerReadWriteAttribute(def, null, writeHandler);   // the 16 survivors
}
... // the 12 removed ones are then registered explicitly, with their own handlers
AliasedHandler hseh = new AliasedHandler(JOURNAL_STORE_ENABLE_ASYNC_IO.getName());
resourceRegistration.registerReadWriteAttribute(HORNETQ_STORE_ENABLE_ASYNC_IO, hseh, hseh);  // LAST
```

`add_attributes` has 28 entries and contains `HORNETQ_STORE_ENABLE_ASYNC_IO`
(index 26). `AttributeDefinition.hashCode()` is `name.hashCode()` and
`AttributeDefinition.equals()` is `name.equals()`, so this is a pure name-keyed
set with no identity component. The `remove` return value is discarded
(`invokeinterface Set.remove; pop`), so only the *effect* matters.

There are therefore exactly two producers:

1. **`attributesWithoutMutuals.remove(HORNETQ_STORE_ENABLE_ASYNC_IO)` did not
   delete its element.** The survivor is registered by the loop AND again by
   the explicit call at the end of the method. Because
   `hornetq-store-enable-async-io` is registered *last* of the twelve, it is
   also the name the exception reports — which is what was observed both times.
2. **`attributes.containsKey(name)` was a false positive.** This needs the
   lookup to match a node stored under a different name — equal stored hash and
   an `equals` that says yes. The only near-twin is
   `journal-store-enable-async-io`: same length (29), same 22-character suffix,
   registered earlier by the loop. But the two spread hashes are 398231451 and
   2622580814, landing in different buckets at every capacity that map reaches
   (11/14 at cap 16, 27/14 at 32 and 64, 27/78 at 128), so this is the far
   weaker of the two.

**Producer 1 is confirmed by construction.** The harness's negative control
(`-Dcvm.dupattr.selftest=1`) skips that one `remove` and changes nothing else;
the boot then dies with output identical to the historical capture, down to the
wrapping `WFLYCTL0079: Failed initializing module org.jboss.as.transactions`.

**This supersedes the previous "next useful discriminator" in this report.**
That section proposed instrumenting the transactions subsystem's
`AliasedHandler`. `AliasedHandler` cannot be the producer: it is only a
read/write `OperationStepHandler` value passed *into*
`registerReadWriteAttribute`, it performs no registration of its own, and it is
constructed after every registration decision has already been made.

## What the earlier exclusions do and do not cover

Two CratonVM double-execution hypotheses were instrumented in earlier campaigns:

- `ParallelExtensionAddHandler$ExtensionInitializeTask.call()` traced by
  receiver and thread — the expected compiler bridge plus covariant method
  produces two entries; no receiver produced a third in 2,400 traced boots.
- `TransactionSubsystemRootResourceDefinition.registerAttributes()` traced by
  receiver and registry identity — no identity pair was invoked twice in an
  800-boot focused campaign.

Both remain valid and are consistent with the localization above: the duplicate
does not come from running the method twice, it comes from one run of the method
registering one name twice.

## Fixes landed on that path

`new HashSet<>(Arrays.asList(add_attributes))` runs entirely in CratonVM
natives — in real-JDK mode `java/util/HashSet.<init>(Ljava/util/Collection;)V`,
`HashMap.put` and `HashSet.remove` are all registered bridges. Auditing that
constructor chain found six places where the only reference to a live object was
a bare Rust local held across an allocation, against the rooting discipline
every neighbouring native already follows:

| function | hole |
|---|---|
| `native_hs_init_from_collection` | `source` carried raw across `alloc_hs_backing`, then dereferenced by `collect_collection_elements_or_real` |
| `native_hs_init` | `this` carried raw across `alloc_hs_backing` |
| `native_hs_init_capacity` | `this` carried raw across `alloc_hs_backing` |
| `native_map_init` (both branches) | `this` carried raw across `alloc_ref_array` |
| `native_map_init_capacity` | `this` carried raw across `alloc_bucket_table` |
| `alloc_hs_backing` | returned the *pre-initialization* reference to a backing map that nothing else references while its initializer allocates |

The first entry's two siblings already carry exactly this guard:
`native_al_init_from_collection` (`src_pin`) and `native_map_init_from_map`
(`source_pin`, added after a live `NoSuchMethodError
java/lang/Object.entrySet()` capture from `new HashMap<>(children)` during
WildFly `parallel-extension-add`). `lhm_init_with_cap` pins across the identical
`alloc_bucket_table` call that `native_map_init_capacity` did not — and
`alloc_hs_backing`'s caller-side local went stale anyway, because that pin only
protected the callee's own copy.

A moving cycle leaves those locals naming from-space; the non-moving sweep
cannot see them as live at all and reclaims-and-zeroes them. These are real
defects — but see the scope limit below before reading them as *the* producer.

## Why the relocation half of that hazard is currently dormant

On current `dev` the young generation does not relocate while any compiled code
exists: `conservative_roots::refresh_moving_young_coverage_for_current_thread`
vetoes moving-young process-wide as soon as `jit_code_range_count() != 0` (see
`docs/known-issues/jit-optimizing-tier-disabled-by-moving-young-default.md`).
Every young collection in a JIT-enabled WildFly boot logs
`[moving-young] fallback … reason=compiled-frame-oop-not-published` and runs the
non-moving sweep. `CRATONVM_DBG_FORCE_MOVING=1` does not override it, because
the coverage-incomplete diversion is checked after the force flag
(`gen_heap.rs`: `if divert_non_moving && (!force_moving || divert_for_incomplete_moving_coverage)`).

So the *relocation* half cannot fire during these boots at all. The *liveness*
half still can — the non-moving sweep reclaims and zeroes young objects the
marker did not reach, and a bare Rust local is not a root — which is why the
fixes above are worth having regardless. It is also the most likely reason the
historical rate was as low as it was.

## Campaign evidence

Harness: `docs/known-issues/repros/wfly0079-dup-attr/`. Real WildFly 32
standalone boots under CratonVM, with WildFly's own
`TransactionSubsystemRootResourceDefinition` recompiled to repeat the suspect
sequence inside the real `parallel-extension-add` thread pool, in three modes —
`warm` (the shared statics, exactly as the real call uses them), `cold` (fresh
`AttributeDefinition`s with fresh name `String`s, so the young-gen and
uncached-`String`-hash profile matches the one real call) and `registry` (the
`HashMap<String, AttributeAccess>` `containsKey`/`put` order from
`storeAttribute`) — plus an always-on check of the single real sequence.

The final configuration runs the canary on 8 concurrent threads. A
single-threaded rep loop reproduces the rate but not the concurrency, and its
later reps run after every other extension has finished, in a quiet VM.

| phase | binary | canary | boots | sequences | hits |
|---|---|---|---|---|---|
| 1 | pre-fix | warm, 1 thread × 3,000 | 27 | 81,000 | 0 |
| 2 | pre-fix | warm+cold, 1 thread × 6,000 | 36 | 216,000 | 0 |
| 3 | post-fix | warm+cold+registry, 1 thread × 6,000 | 21 | 126,000 | 0 |
| 4 | post-fix | warm+cold+registry, 8 threads × 2,000 | 160 | 2,560,000 | 0 |

`CAMPAIGN_PHASE5`

Standalone probes, same result: `WflyAttrSetProbe` 400,000 rounds on the
post-fix binary (8 threads, alternating warm/cold), plus 9,600 rounds spread
over 400 short VM runs — short runs matter because the failure would live in the
cold tier-up window a long loop leaves behind after its first iteration — and
3,291 rounds under `CRATONVM_DBG_GC_STRESS=65536`; `HashSetInitProbe` 20,000
rounds under
`CRATONVM_MOVING_YOUNG=1 CRATONVM_DBG_FORCE_MOVING=1
CRATONVM_DBG_GC_STRESS=262144`. All clean and byte-identical to HotSpot.

**The detectors are not silent by construction.** With
`-Dcvm.dupattr.selftest=1` all three canary modes report, the always-on real
check reports, WildFly throws the genuine `WFLYCTL0043`, and `wfboot.sh`
classifies the boot as a HIT (exit 3) — verified on this exact build before the
final campaign. `WflyAttrSetProbe` has the matching `-Dcvm.probe.selftest=1`
control.

## Reopening

If `WFLYCTL0043` is ever seen again for `hornetq-store-enable-async-io` — or for
any of the other eleven mutually-exclusive attributes: `use-hornetq-store`,
`use-journal-store`, `use-jdbc-store`, `statistics-enabled`,
`enable-statistics`, `default-timeout`, `maximum-timeout`,
`jdbc-store-datasource`, `process-id-uuid`, `process-id-socket-binding`,
`process-id-socket-max-ports`:

1. Turn the canary on (`canary_toggle.sh on`) and run `wfcampaign.sh` with
   `CANARY_THREADS=8` and `reps=2000`. A `CVM-DUPATTR-CANARY FAIL` line names
   the mode, the surviving attribute, both objects' `hashCode()`s, both names'
   `hashCode()`s, and whether the survivor is the identical object that was
   passed to `remove`.
2. `CVM-DUPATTR-REAL FAIL` (always on, no reps needed) means the single real
   sequence failed — the unamplified event, with the same detail.
3. Read the detail:
   * survivor object-identical to the removed argument, hashes agreeing →
     the defect is inside `native_map_remove`'s bucket/chain walk
     (`native-collections/src/lib.rs`, `native_map_remove_pinned`);
   * hashes disagreeing → `String.hashCode` or the node's stored
     `NODE_FIELD_HASH`;
   * `mode=registry` → a `containsKey` false positive instead, i.e. producer 2.
4. Confirm the harness still fires first: one boot with
   `CANARY_SELFTEST=1` must come back HIT.
