# `CopyOnWriteArraySet` was not copy-on-write — the Spring JCache cluster

## Status
**FIXED 2026-08-24.** Both Spring JCache classes go from 0 to equal-to-HotSpot:
**82 test methods recovered.** `probes/CowSnapshotProbe.java` matches HotSpot on
every contract row.

## The failure

```
javax.cache.CacheException: org.ehcache.StateTransitionException
```

and nothing else — the suite runner prints the wrapped exception's message, and
`StateTransitionException`'s message is **empty**, so the entire diagnosis was
discarded before it reached the log. `probes/JCacheProbe.java` (the test's
`@BeforeEach`, standalone) walks the chain instead:

```
[0] javax.cache.CacheException          at Eh107CacheManager.close
[1] org.ehcache.StateTransitionException  at EhcacheManager.removeCache
[2] java.util.ConcurrentModificationException
        at ...terracotta.context.AbstractTreeNode.clean(AbstractTreeNode.java:108)
```

Everything the cache does WORKS — provider, manager, `createCache`, `getCache`,
`put`, `get`, `remove` are all identical to HotSpot. Only `close()` fails, and
it takes every test in both classes with it.

## The mechanism

`AbstractTreeNode.clean()` removes from a collection while iterating it:

```java
for (AbstractTreeNode child : getChildren())   // CopyOnWriteArraySet, wrapped
    removeChild(child);                        // mutates the backing set
```

That is legal *because* `children` is a `CopyOnWriteArraySet`, whose iterator is
a **snapshot**: documented never to throw `ConcurrentModificationException`,
never to reflect later changes, and to refuse `remove()`.

CratonVM's `CopyOnWriteArraySet` is not copy-on-write at all. It shares the
HashSet native surface (`SET_CLASSES` in `native-collections/src/lib.rs`) and is
backed by a live `LinkedHashMap`, so `iterator()` returns that map's
modCount-checked cursor — the exact opposite of the contract on all three
counts.

`CopyOnWriteArrayList` was already correct: `native_al_iterator` has a COWAL
branch that snapshots the `array` field. The **set** never got one.

## The fix

Mirror the JDK's own structure. `java.util.concurrent.CopyOnWriteArraySet` holds
a `CopyOnWriteArrayList` and returns `al.iterator()`; `cow_set_snapshot_iterator`
now does the same — snapshot the elements, mint a real `CopyOnWriteArrayList`
over them, return ITS iterator.

That is deliberate reuse rather than re-derivation: the delegate's iterator is
already measured correct on this VM for snapshot semantics (rows L01-L03) *and*
for refusing `remove()` (row L04), so the set inherits both by construction. If
the delegate ever regresses, the set regresses with it — the intended coupling.

It declines — falling through to the live cursor — for any receiver that is not
a `CopyOnWriteArraySet`, and for the two shapes that would otherwise be guesses:
no resolvable COWAL `array` field, or a failed allocation.

**Placement was the whole of the first attempt's failure.** Hooked before
`resync_view_set`, using the generic `collect_collection_elements`, the snapshot
came back EMPTY and every row read `saw 0` — an iterator that yields nothing is
indistinguishable from a set that is empty, and the probe score went from 8
wrong rows to 12. The check belongs **after** the live path resolves `backing`,
reading elements with `collect_view_snapshot_ordered`, which is what the rest of
this Set surface uses.

## Measurement

`probes/CowSnapshotProbe.java`, 17 rows. Before: 8 wrong. After: **0**.

| row | HotSpot | before | after |
| --- | --- | --- | --- |
| S01-S04 COW set remove/add/clear/removeAll while iterating | `saw 4` | CME | `saw 4` |
| U01-U02 same through `unmodifiableSet` (the EhCache shape) | `saw 4` | CME | `saw 4` |
| N01 snapshot ignores later changes | 4 | CME | 4 |
| R01 `iterator.remove()` | UnsupportedOperation | no-throw | UnsupportedOperation |
| L01-L04 `CopyOnWriteArrayList` (the delegate) | correct | correct | correct |
| **C01 plain `HashSet` MUST still throw CME** | CME | CME | **CME** |
| **C02 plain `ArrayList` MUST still throw CME** | CME | CME | **CME** |

The two **controls** are the point: a VM that simply stopped raising CME would
pass every other row and be badly wrong. They are unchanged.

`M01` (a `ConcurrentHashMap` keySet iterator that sees a concurrent removal)
differs from HotSpot and is **not** a defect — CHM's views are weakly
consistent, so `saw 3` and `saw 4` are both legal. It is labelled as such in the
probe so the next reader is told rather than sent hunting; it is unchanged by
this fix.

Whole classes, HotSpot / before / after:

| class | HS | before | after |
| --- | --- | --- | --- |
| `JCacheEhCacheApiTests` | 15/15 | 0/15 | **15/15** |
| `JCacheEhCacheAnnotationTests` | 67/68 | 0/68 | **67/68** |
| `ContextPathIntegrationTests` | 5/5 | 3/5 | **5/5** |
| `JCacheJavaConfigTests` (control) | 32/32 | 32/32 | 32/32 |

`ContextPathIntegrationTests` was not predicted — Spring's reactive server
infrastructure uses the same collection. It is deterministic, 3/3 runs on each
binary, so it is a real second beneficiary rather than a flake. **84 methods
recovered in total.**

## Regression measurement

A 433-class collection-exposed Spring slice — every class naming a
`Cache`/`Listener`/`Event`/`Multicast`/`Context`/`Registry`/`Collection`/`Set`/
`List`/`Concurrent`/`Lifecycle`/`Scope`/`PostProcessor` concern — swept
ABBA-interleaved, binary the only variable:

| | OK | FAIL |
| --- | --- | --- |
| before | 430 | 3 |
| after | **433** | **0** |

Exactly three rows differ and all three are the improvements above. No
regressions.

In-tree gates against a pristine `c057a3a78` worktree:

| gate | mine | pristine |
| --- | --- | --- |
| `cargo test -p cratonvm-native-collections --lib` | **RC=0** | — |
| `cargo clippy -p cratonvm-native-collections --all-targets` | **RC=0** | RC=0 |
| `cargo test -p cratonvm-vm --lib --features synthetic-jdk` | 21 failing | the SAME 21 |
| `cargo test --workspace` | 19 failing | 19 failing |

The workspace sets differ by one row each way — `the_publishing_heap_still_
clears_its_own_bounds_on_drop` on mine, `d15_attach_socket_threaddump_operation`
on pristine. A symmetric swap of two unrelated tests is the shape of load
sensitivity, and the one on my side passes 3/3 on BOTH trees when run alone.

## How it was found

A sweep of the **1,626 classes** the 2026-08-22 slice excluded — 57% of the
suite, never measured against any binary. It scored 1601/1626 OK and named 25
non-OK classes, of which this cluster was the largest single-cause group. The
earlier Groovy cluster had been found *inside* the other slice by accident; this
one would not have been found at all.

## Still open from that sweep

A ~20-class **reactive HTTP integration** cluster
(`web.reactive.*IntegrationTests`, `http.server.reactive.*`), each failing a
consistent FRACTION of its methods — e.g.
`RequestMappingMessageConversionIntegrationTests` 120/160, `SseIntegrationTests`
33/48. These classes are parameterised over HTTP server backends, so a stable
fraction is the signature of ONE backend failing rather than a scattered defect.
Roughly 130 methods. Not triaged.

Plus four singletons: `ProceedTests` 3/4, `RetryPolicyTests` 22/23,
`ExecutorSubscribableChannelTests` 9/10, and
`FileNativeConfigurationWriterTests` (already recorded as not-a-CratonVM-bug).

## Repro

```bash
javac -d /tmp/cls probes/CowSnapshotProbe.java
java -cp /tmp/cls CowSnapshotProbe > /tmp/hs.txt
<cratonvm-bin> --java-home $JDK25 -cp /tmp/cls CowSnapshotProbe 2>/dev/null | diff /tmp/hs.txt -
```

Diff on **stdout only** — CratonVM's tracing goes to stderr.
