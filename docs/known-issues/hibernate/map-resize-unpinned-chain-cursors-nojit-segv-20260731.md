# `map_resize_inner` publishes stale chain heads into the resized bucket array — permanent map corruption, SIGSEGV under `--nojit`

| | |
|---|---|
| **Status** | 🔴 **STILL OPEN.** The `map_resize_inner` pin refactor this doc called for is now landed and closes a genuine GC-safety hole — but it does **NOT** stop the SIGSEGV. See "Follow-up 2026-07-31" at the bottom: a second corruptor, in the *put* path, is the dominant one. |
| **ID** | `HIB-MAPRESIZE-STALE.1` |
| **Found** | 2026-07-31, while validating the `DefaultCatalogAndSchemaTest` runner accommodation (see [`../../internal/fixed-suite-bugs/hibernate/qualfiedtablenaming-runner-timeout-floor-lost-20260731-FIXED.md`](../../internal/fixed-suite-bugs/hibernate/qualfiedtablenaming-runner-timeout-floor-lost-20260731-FIXED.md)). |
| **Repro** | `org.hibernate.orm.test.boot.database.qualfiedTableNaming.DefaultCatalogAndSchemaTest` under `--nojit`, `--Xmx 1500m`, real JDK. SIGSEGV (rc=139) at ~17–20 min, three for three. |

## Symptom

```
$ cratonvm.exe --java-home <jdk25> --Xmx 1500m --nojit @common.args \
      -Dcraton.batch=1 CratonRunner org...DefaultCatalogAndSchemaTest
... rc=139 (Segmentation fault), no @@RESULT ever printed
```

Preceded by hundreds of dropped out-of-bounds field writes:

```
gen_heap::set_field: out-of-bounds field write dropped
  index=3 num_slots=0 class_id=ClassId(0) class_name=java/lang/Object
```

`index=3` is `NODE_FIELD_NEXT`; `num_slots=0` means the receiver is not a map
node at all any more. Reproduced three times: 174, 606 and 375 drops.

HotSpot runs the identical class, classpath and heap in **119.7 s**,
`found=132 ok=132 failed=0`, clean — so this is not heap pressure.

## Where

Two call sites appear in the captured backtraces, and the second is a
consequence of the first:

| frame | file:line | what it writes |
|---|---|---|
| `map_resize_inner` | `native-collections/src/lib.rs:5806` / `:5809` | `set_field(lo_tail/hi_tail, NODE_FIELD_NEXT, null)` |
| `native_map_put_evict_pinned` | `native-collections/src/lib.rs:7209` | `set_field(tail_node, NODE_FIELD_NEXT, new_node)` |

reached from `native_chm_put` → `native_map_put_evict` → `safe_native_call_impl`.

## Root cause

`map_resize_inner`'s JDK-style split walk (`native-collections/src/lib.rs`,
~5725–5814) partitions each old bucket's chain into a "low" and a "high" list,
tracking four cursors:

```rust
let mut node_val = ctx.get_array_element(old_b, i);
let mut lo_head: Option<ObjectRef> = None;
let mut lo_tail: Option<ObjectRef> = None;
let mut hi_head: Option<ObjectRef> = None;
let mut hi_tail: Option<ObjectRef> = None;
while let Value::Object(Some(node)) = node_val {
    ...
    } else if let Some(t) = lo_tail {
        ctx.set_field(t, NODE_FIELD_NEXT, Value::Object(Some(node)));   // <-- GC point
    }
    lo_tail = Some(node);
    ...
}
...
ctx.set_array_element(new_buckets, i, Value::Object(lo_head));          // <-- publishes it
ctx.set_array_element(new_buckets, i + old_cap as usize, Value::Object(hi_head));
```

Those two `set_field` calls are **reference-typed stores into a chain node**, so
each can allocate a remembered-set entry through the write barrier and therefore
complete a moving young GC. This is not speculative — it is the exact hazard the
sibling `native_map_put_evict_pinned` documents in its own comment a few hundred
lines below, and pins against with `pin_native_root` / `read_native_pin`:

> *"the reference-typed `set_field`/`set_array_element` writes just above can
> each trigger a write-barrier remembered-set allocation (and therefore a moving
> young GC) when linking a young `new_node` into an old-generation
> chain/bucket array — confirmed live via `CRATONVM_DBG_STALE_OBJREF`"*

`map_resize_inner` never got that treatment. `old_b`, `new_buckets`,
`node_val` and all four cursors are bare Rust locals, invisible to
`collect_roots`. After a GC inside the loop they name pre-move addresses, and
the code then:

1. writes `NODE_FIELD_NEXT` through a freed `lo_tail`/`hi_tail` — the
   `index=3 num_slots=0` drop, caught by the `gen_heap` guard; and, far worse,
2. **publishes the stale `lo_head`/`hi_head` into `new_buckets`**, so the map
   permanently holds dangling chain heads.

Step 2 is why the crash is delayed and why the second call site appears: every
later `put` walks a chain of freed nodes, so `native_map_put_evict_pinned`'s
`tail_node` is a genuinely dead object by the time it is pinned — pinning a
stale reference preserves the staleness. Eventually a walk dereferences memory
that is no longer mapped and the process dies.

The same unpinned-cursor pattern is present in the cycle fallback (~5784–5801)
and the legacy rebuild path (~5817+).

## Why it does not show up in the JIT-on lane

With JIT on, `gc_quiescence` fails closed to the **non-moving** young sweep
whenever a thread holds a live JIT frame (`[moving-young] fallback #N:
reason=compiled-frame-oop-not-published` appears in these runs). A non-moving
sweep does not relocate survivors, so the stale-cursor window never opens.
`--nojit` removes the JIT frames, real moving collections run, and the bug
becomes reachable. The JIT-on lane has its own separate blocker — see the
companion doc.

It also never surfaced in the 4548-class `categorize-20260730-225515` run: every
class there is killed at the flat 300 s cap, and this fault needs ~16 min of
sustained GC pressure. Zero occurrences of the signature across all 8 shards.

## Suggested fix

Give the split walk the same pin discipline `native_map_put_evict_pinned`
already has: pin `old_b`/`new_buckets` for the bucket pass and the four cursors
plus `node`/`next` across each step's reference store, re-reading every one
through its handle afterwards. Two cautions found while prototyping it:

- The pin API is a **stack** (`unpin_native_roots(base)` releases `base` and
  everything above), so the per-step cursor frame must be taken *above* the
  array pins and torn down at the end of the step.
- The `old_buckets`/`new_buckets` bindings in the enclosing scope must be
  written back after each bucket pass. Re-reading into a per-iteration shadow
  leaves the outer binding stale, so bucket `i+1` would pin an already-stale
  address — a subtler version of the same bug.

Not landed here: this is a hot native (every `HashMap`/`ConcurrentHashMap`
resize), the change is ~50 lines of pin plumbing, and each validation cycle is a
10-minute build plus a ~50-minute run. It deserves its own change with its own
`CRATONVM_DBG_GC_STRESS` reproducer rather than being folded into an unrelated
harness fix.

## Follow-up 2026-07-31 — the refactor landed; the crash did not go away

`codex/fix-map-resize-stale-cursors-20260731` implements the pinning described
above, across the split walk, the cycle fallback and the legacy rebuild path,
and additionally re-reads `this`/`new_buckets` before the publication writes
(they were pinned once before `alloc_ref_array` and never refreshed, so the
final `set_field_volatile(this, MAP_FIELD_BUCKETS, ..)` could itself target a
pre-move address). It is non-regressive: `regression-suite/run.sh` reports
19 passed / 0 failed, all HotSpot-diffed, including a new `RMapResizeGc`.

**The mechanism above needs one correction.** The receivers read
`num_slots=0 class_id=ClassId(0)` — *zeroed* memory, not merely relocated
memory. `native_pin_roots` is both enumerated as a GC root
(`vm/src/memory/roots.rs:229`) and remapped after a collection
(`vm/src/memory/gc.rs:602`), so an unpinned cursor is not just stale, it is
**not a root at all**: once the walk rewrites a predecessor's `NEXT`, the
detached "high" partition is reachable *only* from the native locals, and a
collection there **reclaims it outright**. That is what produces the zeroed
receiver, and it is why pinning (which roots *and* remaps) is the right fix.

**But the SIGSEGV survives the fix.** A/B on the reproducer, both arms from the
same tree, run concurrently on the same host:

| arm | outcome | drops | backtraces | tests reached |
|---|---|---|---|---|
| baseline | SIGSEGV rc=139 @ ~36 min | 0 | 0 | 94/132 |
| + pin refactor | SIGSEGV rc=139 @ ~29 min | 91 | 6 | 74/132 |

Read this table carefully rather than optimistically:

- The baseline logged **zero** drops this run, so it yields **no** drop-site
  distribution to compare against. The earlier claim that the refactor removes
  `map_resize_inner` from the corrupt-write sites is **not supported** by this
  A/B — it is merely absent from the fixed arm's 6 sampled backtraces.
- Run-to-run variance is enormous: drop counts across five runs of this
  reproducer have been 0, 91, 174, 375 and 606, and tests-reached 74–104.
  Single runs cannot settle anything subtle here; the 74-vs-94 difference is
  inside that band and should not be read as the fix hurting.
- The one sampled site in the fixed arm is
  `native_map_put_evict_pinned` (`native-collections/src/lib.rs`), reached from
  `native_chm_put`, which matches the earlier baseline's 3-of-5 majority.

**Prime remaining suspect** — `native_map_put_evict_pinned`'s chain walk. It
assigns `tail_node = Some(node)` at the top of each iteration and refreshes it
after each `map_keys_equal` (arbitrary Java `equals()`, a GC point) — but the
walk also calls `map_resize` *before* it, and `buckets`/`idx` are computed
around that call. Pinning `tail_node` at the end (as the code does) cannot help
if the value pinned was already stale, or if the bucket array it was reached
through was replaced by the resize. That is the next thing to instrument, with
`CRATONVM_DBG_BLOCKGC` (its pin-time canary fires exactly on "pinned an
already-forwarded address", naming the upstream culprit).

**Do not close this doc on the strength of the landed refactor.**

## Follow-up 2 — 2026-07-31: relocation is RULED OUT as the mechanism

Went after `native_map_put_evict_pinned` as this doc directed. The most useful
result is a **negative** one, and it invalidates the framing used above.

**The corrupt receiver is `tail_node`.** Every drop from this site reports
`index=3`, and `NODE_FIELD_NEXT == 3`, so the failing store is the tail-append
`set_field(t, NODE_FIELD_NEXT, Some(new_node))` — `t` is the `tail_node` the
chain walk produced.

**But `tail_node` is not stale.** A full failing run (`CRATONVM_DBG_BLOCKGC=1`,
SIGSEGV rc=139, 93/132 tests) recorded **zero** `[blockgc] PIN-STALE` hits.
That canary fires whenever *any* `pin_native_root` receives an already-forwarded
address — precisely the "pinned a stale value, so the pin preserved the
staleness" failure hypothesised above. Not one fired, anywhere in the process,
across the whole run.

That rules out the entire stale-`ObjectRef` family for this crash:

- `tail_node` is pinned before `alloc_object` and re-read from that pin
  immediately before the store, and `native_pin_roots` is a genuine GC root, so
  it cannot be collected *after* the pin either.
- With no forwarding recorded anywhere, it was not silently pre-stale when
  pinned.

**So `tail_node` was already a dead, zeroed node when the walk read it out of
the chain.** `native_map_put_evict_pinned` is a *victim* site, not the source:
something reclaims a node that is still linked into a live bucket chain, and
this store is merely where the damage first becomes visible. Chasing this
function further is chasing a symptom.

**Where to look next.** The source is a *rooting/liveness* defect, not a
staleness one, so the tooling has to change:

- `CRATONVM_DBG_BLOCKGC`'s pin-time canary only sees relocation — wrong
  detector.
- A `CRATONVM_DBG_GC_STRESS` probe is also wrong unless moving-young actually
  engages: one built for this reported `moving_young: cycles=0
  coverage_fallbacks=0`, i.e. the mechanism never ran, so its passing on an
  *unfixed* binary proved nothing (kept as `regression-suite/src/RMapResizeGc.java`
  for its HotSpot-diffed coverage, but it is NOT a reproducer).
- What is wanted is a liveness/ownership assertion: mark nodes at link time and
  check at sweep time that nothing reachable from a live bucket array is being
  reclaimed.

**Also landed (hardening, NOT the fix): `HIB-MAPPUT-PINORDER.1`.**
`native_map_put_evict` and `native_hashmap_put_exact` copy `key_val`/`value` out
of `args` into bare locals, then call `materialize_hm_int_fast` — which re-puts
every side-stored entry through `native_map_put_evict_pinned`, allocating a node
each, so it can collect — and only pin them *afterwards*. Pinning after a
collection roots an already-stale address. Given the canary result this is
**latent, not live** on the collector that actually runs today, but it becomes
live the moment moving-young engages (its own open doc). Fixed by pinning across
the materialisation and re-reading after it.

## Follow-up 3 — 2026-08-01: the victim has NO heap referrer at sweep time

A freed-while-referenced assertion (`CRATONVM_DBG_SWEEP_LIVENESS`, on
`codex/gc-sweep-liveness-assert-20260731`) now covers every heap→heap edge into
a block a sweep is about to reclaim, and all of them come back clean on this
reproducer. Five detectors, five negatives, each from a full failing run:

| # | detector | covers | result |
|---|---|---|---|
| 1 | `CRATONVM_DBG_BLOCKGC` PIN-STALE | any pin of an already-forwarded address | 0 hits |
| 2 | `SWEEP-LIVENESS` (old-gen sweep) | marked old-gen -> doomed old-gen | 0 hits |
| 3 | `SWEEP-LIVENESS young` | old-gen -> doomed young span | 0 hits |
| 4 | `SWEEP-LIVENESS young` | young survivor -> doomed young span | 0 hits |
| 5 | all of the above armed together | — | SIGSEGV 94/132 anyway |

**What that eliminates.** "The mark missed a heap edge" is now excluded in both
generations and both directions. At the moment a block is reclaimed, no live
heap object points at it — so from the collector's point of view the reclaim is
CORRECT.

**What that leaves.** The only referrer is something that is neither a heap
object nor a GC root: a native-side Rust local, a side table, or a cache. The
node genuinely became garbage while native code was still using it — the same
family as `HIB-MAPRESIZE-STALE.1` and `HIB-MAPPUT-PINORDER.1`, both fixed on
this branch, which says at least one more unpinned holder is still out there.
That reframes the hunt a third time: not "which root did the mark miss" but
"which native local is the last reference".

**Caveat, stated plainly.** Each row is ONE run of a high-variance failure —
this reproducer has produced drop counts of 0, 91, 174, 375 and 606 and crashed
anywhere between 74 and 104 tests. Run 5 produced no dropped writes at all, so
the observable corruption signal was absent even though the process still died.
A clean detector on one run is evidence, not proof; these detectors are cheap to
re-arm and worth re-running before treating the eliminations as final.

**Next probe, concretely.** Stop asking the collector and start asking the
allocator: record every node address the map natives allocate together with its
holder, and have the sweep report when it reclaims one whose holder has not
released it. That turns "some native local" into a named call site, which is
what three rounds of collector-side detectors could not do.

## Related

- [`reference_native_fastpath_reimplements_library_method_semantics`] family —
  native fast paths that own a library method's whole semantics also own its
  GC-safety obligations.
- `docs/known-issues/wildfly-parallel-boot-stale-objectref-residual.md` — the
  same stale-`ObjectRef`-across-a-native-callback class of bug, in the put path
  rather than the resize path.
- `HIB-WEAKREF-RECYCLE.1`, fixed in the companion doc: a *different* corrupt
  writer in the same runs, in post-GC reference processing. Fixing it removed
  84 bad writes per run and eliminated `process_references_after_gc` from these
  backtraces entirely, but did not stop this one.
