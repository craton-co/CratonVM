# `ObjectCleanerTest`'s `AbstractMethodError` was a STALE TLAB skip span hiding a live object from the sweep

**Status:** ✅ RESOLVED (2026-09-06). Retired from
`docs/known-issues/netty/objectcleanertest-abstractmethoderror-comparator-intermittent-20260905.md`,
filed 2026-09-04 as "OPEN, unreproduced".

| arm | before | after |
|---|---|---|
| `-XX:+UseGenerationalGC` | **8/8 non-clean** | **0/8** |
| `-XX:+UseG1GC` | **8/8 non-clean** | **0/8** |
| `-XX:+UseG1GC --nojit` | 4/5 non-clean | 0/10 |
| default / `-XX:+UseZGC` | 0/5 | 0/10 |

`io.netty.util.internal.ObjectCleanerTest` now reports `found=3 ok=3 failed=0`
on every collector. 50 runs across five arms, all clean.

---

## 1. The one-line cause

A **published TLAB skip span was stale**, so the young sweep skipped a range of
the arena that contained a **live object**, and `mark_young`'s anchor oracle
answered *"free/gap space, not an object"* for the root pointing at it. The
object was therefore neither scanned nor swept: it survived, unmarked, while
**everything it referenced was freed underneath it**.

The repair is one statement:

```rust
// vm/src/runtime/interpreter/gc_and_alloc.rs
let regions = shared.threads.thread_registry.collect_reserved_tlab_tails();
shared.mem.heap.set_jit_tlab_skip_regions(&regions);   // was: `if taken.count() > 0 || ...`
```

## 2. Why the span went stale

A skip span is the still-**reserved, un-allocated** tail `[cursor, end)` of a
thread's TLAB. The sweep's linear walks resync past it (BUG-03: an un-retired
tail of a forcibly-stopped in-JIT peer would otherwise desync the walk).

The set is **process-global and survives the collection that wrote it**. It is
"cleared by the caller after the collection completes" at **seven** separate
exits — and the publish was guarded:

```rust
if taken.count() > 0 || helper_windows > 0 || !regions.is_empty() { … }
```

So a path that missed one of the seven clears left the previous collection's
spans in place, and the guard then declined to overwrite them **precisely when
`regions` was empty — i.e. exactly when they were stale**. Meanwhile the owning
thread had resumed and bump-allocated into `[cursor, end)`, so the span now
covered live objects.

Publishing the current set unconditionally makes the span always describe *this*
collection, and keeps BUG-03's protection for a genuinely un-retired tail.

## 3. How that reached netty

JUnit's `NamespacedHierarchicalStore$EvaluatedValue` has

```java
private static final Comparator<EvaluatedValue<?>> REVERSE_INSERT_ORDER =
        Comparator.comparing(EvaluatedValue::getOrder).reversed();
```

`reversed()` is `Collections.reverseOrder(cmp)`, i.e. a
`Collections$ReverseComparator2` whose single field holds the `comparing(...)`
lambda. The comparator object landed inside a stale skip span. At the next young
collection:

* it was **in the root set** (`holder_in_roots=true`) — reached from the static;
* it was **compact with a correct oop map** (`ref_offsets=[0]`);
* and `mark_young` still dropped it, because the oracle's `verified_spans` are
  built from the same skip list and said its address was gap space.

Its `cmp` lambda was therefore never traced, was swept and zeroed, and the next
`compare` through the still-live comparator dispatched on an all-zero header:

```
java.lang.AbstractMethodError: method java/util/Comparator.compare(Ljava/lang/Object;Ljava/lang/Object;)I has no Code attribute
	at java.util.Collections$ReverseComparator2.compare(Collections.java:5756)
	at org.junit.platform.engine.support.store.NamespacedHierarchicalStore.close(...:136)
```

Two more faces of the same reclamation appear in the same run
(`Predicate.test`, `MutableExtensionRegistry$Entry.getExtension`, both
`recv_cid=0`), which is why the class reports `found=3 started=2` — the JUnit
engine dies mid-class.

### ZGC, which is immune for a different reason than this page first gave

The first version of this page said *"ZGC is immune because it does not use
this sweep"*. The first half is the conclusion and it holds — ZGC measured 0/5
before the fix and 0/10 after. The second half is **wrong**, and worth
correcting because it is the kind of wrong that licenses a future change.

ZGC does consume the same process-global list. `set_jit_tlab_skip_regions`
fans out to all three backends (`vm_heap.rs`), and ZGC's complement sweep reads
it at `zgc/sweep.rs:450`.

What saves ZGC is not that it abstains — it is the **polarity** of its two
uses, both of which withhold bytes rather than hand them out:

* `withhold_skip_regions` clips published tails **out of the free spans**, so a
  stale span means some bytes are not offered to anyone; and
* the bump cursor is raised to `jit_tlab_skip_floor`, so a stale span means the
  cursor does not retract as far as it could.

A stale span therefore costs ZGC **space, not correctness** — a small leak that
the next cycle's unconditional publish corrects. Decisively, **nothing on ZGC's
marking side reads the list at all**: liveness comes from the mark bitmap, so
no root is ever answered *"gap space, not an object"*. That is precisely the
mechanism that made the generational and G1 case a use-after-free, and ZGC does
not have it.

So the invariant is worth stating for ZGC too, even though the guard is not
wired there: **if ZGC ever consults this list on the marking side, or uses it
to skip bitmap ranges, it inherits the defect and needs the same check.** The
guard was left out because a fire would name a real stale span but not a real
ZGC bug, and a guard that reports things the collector is immune to trains
people to ignore it.

## 4. The A/B, and why the page said "unreproduced"

`CRATONVM_GC_CONDITIONAL_TLAB_SKIP_PUBLISH=1` restores the pre-fix guard, in one
binary:

| arm | non-clean | skip-span guard fired | label it printed |
|---|---:|---:|---|
| Generational, fixed | 0/8 | 0/8 | — |
| Generational, `CONDITIONAL_TLAB_SKIP_PUBLISH=1` | **8/8** | **8/8** | `backend="generational"` |
| G1, fixed | 0/8 | 0/8 | — |
| G1, `CONDITIONAL_TLAB_SKIP_PUBLISH=1` | **8/8** | **8/8** | `backend="g1"` |

The original page recorded 16 clean follow-up attempts. **All 16 ran the default
collector**, which is ZGC. One arm per collector would have found it in three
seconds. See `a-per-collector-sweep-finds-bugs-the-default-cannot-reach`.

## 5. The new guard

`SKIP_SPAN_ROOT_VIOLATIONS` — unconditional, in both non-moving sweeps: **a
root pointing into a published skip span disproves the span.** `roots` is the
exact set the mark phase is about to be given, so the question is decidable
right there and costs one pass over the roots against a list with at most one
entry per live thread. It reports at `error` level on the `cratonvm::gc::guard`
target and names the root, the span and the count.

It **reports and does not repair**: dropping the span would put the walk back on
bytes that may genuinely be an un-retired tail — the desync BUG-03 introduced
skipping to prevent — and the guard cannot tell a stale span from a live one,
only that *this* one is stale. The publish side establishes the invariant; this
checks it.

### The G1 twin, and why it is one function and not two

The first cut of the guard lived in `gen_heap.rs`'s sweep alone, so it fired
8/8 on the Generational pre-fix arm and **0/8 on the G1 one — on runs that were
failing 8/8**. A check present in one sweep and absent from the other does not
read as "not implemented here"; it reads as *"the collector is fine here"*, and
that reading was available for as long as the asymmetry stood.

It is now **one function**, `heap::skip_spans_hold_no_root`, called from both:
`sweep_young_non_moving` and G1's `collect_garbage` — in each case the
once-per-collection site that has the roots in hand, before any of them are
followed. The only real difference between the backends is the span
representation, and it is a difference worth naming because copying the
arithmetic across would have been silently wrong: **`gen_heap` holds skip spans
as young-from `(offset, size)` pairs and G1 holds them as absolute
`[start, end)`**. The generational caller converts; the shared check takes
absolute pairs only. A second copy would have had to re-derive that, and a
guard that quietly never matches is worse than no guard, because it reports a
zero.

Each fire now names the sweep it came from:

```
ERROR cratonvm::gc::guard: a ROOT points into a published TLAB skip span …
      backend="g1" roots_in_spans=46 root="0x24d66a40148" span="0x24d66a3c068+0x8ab0" spans=1
```

Re-measured on one binary, 8 reps per arm: the G1 pre-fix arm moves
**0/8 → 8/8** and each arm's logs carry only its own label, so no fire is
credited to the wrong sweep. Both fixed arms stay `0/8 bad, guard_fired=0/8` —
the guard is silent when the invariant holds.

## 6. What it took to find, and the instruments that lied

Nine hypotheses were refuted before this one (full table on the retired page):
conservative-scan false positive, the JIT (`--nojit` reproduces), young
relocation, an object-grid desync, an old→young card miss, a live heap holder,
a root in a freed span, a native holding it in a Rust local, the static-field
`metadata_pin` deferral, the root-snapshot cache, and forced conservative
locals. Three instruments actively misled:

* **A flat word scan of the young arena invents holders.** An inverted holder
  scan reported 240 "live holder → doomed" edges; walking the same arena **as
  objects** reports 0. Free/gap words are not holders.
* **`CRATONVM_DBG_RSET_AUDIT` prints only when it finds edges.** Silence is
  `edges == 0` — a vacuous "no misses", not a refutation.
* **A walk that breaks early still prints a count.** The class trace reported
  "0 instances of `ReverseComparator2`" at the failing cycle. It now reports
  `COMPLETE` or `TRUNCATED at off=N — the count is a LOWER BOUND`, and the
  finding only became trustworthy once it said `COMPLETE`.

What actually closed it was arming a watch on the object **at its allocation**
(`CRATONVM_DBG_WATCH_ALLOC_CID=<hex class id>`, added for this — a lambda proxy
has no class name for `CRATONVM_DBG_MARK_WHY_CLASS` to key on) and then reading
one line from the sweep:

```
[MARKWHY] skip-arm probe: watch=… off=0x52d508 used=0x57bc28
          in_free_block=None in_jit_tlab_skip=Some((5417912, 35504))
```
