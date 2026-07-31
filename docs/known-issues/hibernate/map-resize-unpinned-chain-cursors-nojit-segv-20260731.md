# `map_resize_inner` publishes stale chain heads into the resized bucket array — permanent map corruption, SIGSEGV under `--nojit`

| | |
|---|---|
| **Status** | 🔴 OPEN — real VM defect, root cause located, fix not landed. |
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
