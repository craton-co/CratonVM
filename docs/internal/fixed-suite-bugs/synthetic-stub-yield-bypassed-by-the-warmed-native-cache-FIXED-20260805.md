# A warmed native target bypassed the `SyntheticStub` yield-to-real-bytecode arbitration

**Status: FIXED 2026-08-05.** Found by a native-registry census, not by a
failing test — every affected call still produced the right answer.

## What was wrong

`real_protected_stub_class_common` lists eleven classes whose real JDK bytecode
must win over CratonVM's approximate `SyntheticStub` natives once that bytecode
is loaded. On a Spring Boot run, six triples on that list dispatched the stub
anyway:

| triple | stub dispatches |
|---|---:|
| `AtomicBoolean.compareAndSet(ZZ)Z` | 2 889 |
| `AtomicBoolean.get()Z` | 1 175 |
| `AtomicBoolean.set(Z)V` | 538 |
| `Instant.getEpochSecond()J` | 160 |
| `Instant.getNano()I` | 159 |
| `AtomicBoolean.getAndSet(Z)Z` | 6 |

## Why the obvious two explanations were both wrong

The predicate could have been unreachable on these paths, or `has_real` could
have been computing `false`. Neither. `CRATONVM_DBG_STUB_YIELD` traces every
arbitration and the term that decided it, and on both an isolated probe and the
full Spring run **every** answer was `yield=true — real bytecode wins`. The
arbitration was right every time it was asked.

A tempting third explanation also died here. The predicate tests
`m.code().is_some()`, and `code()`'s own doc says it returns the attribute
"if present and **already decoded** — lazy attributes must be force-decoded by
the caller first", which is precisely the decode-state artefact the census
documentation warns against and avoids by deriving `has_code` from access flags.
That would have been a satisfying bug. It is not this one; the trace shows the
predicate answering correctly, so it was never the term that decided.

## The actual path

Tagging all nine `record_invocation` sites with a `[STUB-DISPATCH] site=…`
marker put every one of the probe's 597 dispatches in a single place:

    597 [STUB-DISPATCH] site=revalidate_cached_native:COMPATIBLE-FAST-RETURN

A native target is published once per call site and then redeemed forever.
`revalidate_cached_native`'s `Compatible` arm redeemed the cached callback and
counted it, with no re-arbitration — its own comment said "a published native
target has already won this call site's compatibility decision". It had, but at
a moment when the real class was not yet loaded, so the arbitration then
answered "do not yield" and the stub was pinned for the rest of the run. The
handful of `yield=true` lines in the trace are the cold calls that still
arbitrate; the thousands of dispatches are warmed sites that no longer do.

**A cache that memoizes a decision must re-ask it when the decision's inputs can
change.** "Class is loaded" changes exactly once, in the direction that
invalidates the memo.

## The fix

`revalidate_cached_native` now revalidates. Returning `None` is the eviction
signal every caller already implements (`invoke_cache.evict(…)` then
`CacheMiss`), so the site re-resolves through a path that arbitrates.

Hot-path cost is one integer compare: only a `SyntheticStub` gets as far as
materialising its triple, and only the eleven allow-listed classes reach the
class manager.

| | before | after |
|---|---:|---:|
| allow-listed stub dispatches | 4 927 | **0** |
| allow-listed stub-over-real-bytecode rows | 6 | **0** |
| `CacheAutoConfigurationTests` | 59/59 | 59/59 |

## The second defect the census found: an ambient kind downgrades a chosen one

`NativeMethodRegistry::current_category` defaults to `SyntheticStub` and is
ambient state. Of the 72 slots dispatching a `SyntheticStub` over loaded real
bytecode, **not one** had `kind_stated`, and **24** had displaced an explicitly
categorised `Bridge` or `Intrinsic` — every `StampedLock` method, all of
`ServiceLoader`, `CountDownLatch`, seven `Set.of` arities, and
`StreamSupport.stream`, whose registrar re-registers the triple purely to win
last-write-wins on the *callback* and says so in its own comment.

That is not cosmetic. `NativeKind` decides three separate things:
`CompatibilityMode::JdkOnly` **refuses** a `SyntheticStub`, `CRATONVM_NO_STUBS`
**drops** one, and only a `SyntheticStub` is subject to the yield arbitration at
all. A bridge mis-tagged this way stops dispatching under `--jdk-only` while its
stated original would have been allowed.

Rule: **an ambient category is "no opinion", and no-opinion must not overwrite
an adjudicated one.**

### The first cut of that rule was wrong, and the census caught it

Keyed on the census's `kind_stated`, which only `register_with_kind` sets. But
`set_category` and `with_category` are choices too — `with_category`'s own doc
calls it "how a whole `register_*` function tags all of its registrations". So
the rule preserved a stated `Bridge` over a deliberate
`with_category(Intrinsic)` re-registration of `java/lang/Float.intBitsToFloat`:
the same downgrade the rule exists to prevent, pointing the other way.

Diffing `--dump-native-registry` across the change showed exactly one kind had
moved, and it was that one. Re-keyed on a new `category_chosen`, which all three
entry points set; the final census shows **zero** kinds moved.

**Run the census diff across your own fix, not just before it.** A rule about
which registration wins is exactly the kind that mis-fires in the mirror
direction, and nothing else in the build would have said so.

## Deliberately NOT changed

66 further triples dispatch a `SyntheticStub` over loaded real bytecode —
`StampedLock`, `Collections`, `List.of`, `Comparator`, `StreamSupport`, the
Spring bootstrap fast paths. **None is on the allow-list**, so current policy
permits them and they are not bypasses. Whether each *should* be protected is a
policy question, and `real_protected_stub_class`'s own doc already names the
intended endgame: "what must ultimately replace this list: `NativeKind` alone —
under `--jdk-only` a `SyntheticStub` never dispatches, so no class needs
protecting from one and the entire allow-list becomes dead."

## Reproducing

```bash
cratonvm --dump-native-registry out.json ...   # any Spring Boot workload
```

then filter `natives[]` for `kind == "synthetic-stub" && invocations > 0 &&
real_declaring_method.has_code`, and intersect the classes with
`real_protected_stub_class_common`. `CRATONVM_DBG_STUB_YIELD=1` traces each
arbitration and the term that decided it.
