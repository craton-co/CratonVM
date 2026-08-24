# Natives holding an unpinned reference across a GC-capable call: 32 candidates

## Why this list exists

`bug-generational-ntru-unpinned-jit-reference-20260821-FIXED.md` cost about a
week. The defect turned out to be one shape, stated in one sentence:

> a native holds a raw `ObjectRef` / `Value::Object` in a Rust local across a
> call that can allocate, and dereferences it afterwards.

The caller's operand slot is a root and gets remapped; the native's **copy** is
not and does not. When a collection lands inside that window the copy is stale,
and the next read sees an all-zero header — which the class manager names
`java.lang.Object`. That is the whole of `d != java.lang.Object`.

The shape is worth finding again cheaply. This is that search.

## The convention is already in place

The codebase overwhelmingly does this correctly — `pin_native_root` to retain,
`read_native_pin` to re-derive after the call:

```text
pin_native_root    4929
read_native_pin    4001
unpin_native_roots 2216      across 97 files
```

Two structural anti-patterns were checked first and both came back clean:

| check | flagged | verdict |
|---|---|---|
| pins but never re-derives | `unsafe_natives.rs` | correct — retention-only across `park()`, never dereferenced after |
| pins but never releases | `graalvm_compat.rs` | correct — `ImageSingletons` are deliberately permanent roots |

So the gap is not "nobody pins". It is the handful of sites that pin *nothing*.

## The candidates

Heuristic: within one scope, an object-typed local bound **before** a
GC-capable call (`ctx.invoke_*`, `ctx.new_array`, `ctx.new_object`,
`create_java_string`) and used **after** it, in a body containing no
`pin_native_root` at all. **32 functions, 41 bindings.**

The cleanest shape is allocate-then-store:

```rust
// native_collections_singleton_list, phases_early.rs:364
let elem = args.first().copied().unwrap_or(Value::Object(None));
if let Ok(cid) = ctx.ensure_class_initialized("java/util/Collections$SingletonList") {
    let list = ctx.alloc_object(cid, ctx.class_num_total_fields(cid));
    ctx.set_field_by_name(list, "element", elem);   // <- elem may be stale
```

`ensure_class_initialized` can run a `<clinit>` — arbitrary Java, arbitrary
allocation — and `alloc_object` allocates. `elem` is a Rust-local copy of an
argument, unpinned across both, and is then stored into a fresh object.
Structurally identical to the `format_impl` defect.

Others of the same shape, by file:

| file | function | holds | across |
|---|---|---|---|
| `phases_early.rs` | `native_collections_singleton_list` | `elem` | `new_array` |
| `phases_early.rs` | `native_collections_singleton_map` | `key`, `val` | `new_array` |
| `phases_early.rs` | `native_bs_init` / `native_bs_init_nbits` | `this` | `new_array` |
| `phases_early.rs` | `native_es_add_all` | `elems` | `invoke_virtual(coll,"toArray")` |
| `message_digest.rs` | `md_update_bytebuffer` | `buf`, `this` | `invoke_virtual(buf,"remaining")` |
| `message_digest.rs` | `md_digest_input`, `md_to_string` | `this` | `invoke_virtual` |
| `key_factory.rs` | `kf_translate_key` | `key` | `invoke_virtual(key,"getAlgorithm")` |
| `jdk25_concurrency.rs` | `native_carrier_run` / `_call` | `result` | `invoke_virtual(runnable,"run")` |
| `jdk25_concurrency.rs` | `native_fork_runner_run` | `call_result` | `invoke_virtual(callable,"call")` |
| `http_url_connection.rs` | `huc_init` | `this` | `invoke_virtual(url,"toExternalForm")` |
| `locale_resources.rs` | `rb_get_object` | `key` | `invoke_virtual(this,"getContents")` |
| `lucene_es.rs` | `native_es_knn_score_doc_query_init` | `this` | `new_array` |
| `lib.rs` | `native_response_to_absolute` | `location` | `invoke_virtual(request,…)` |

`native_carrier_run` is worth its own look: the call it holds `result` across is
a user `Runnable.run()`, i.e. unbounded application code.

## What this is NOT

* **Not a defect list.** Every entry is *structurally* like the fixed bug. None
  has been shown to fail. A candidate is only real if a collection can land in
  that window on a reachable path AND the later use dereferences.
* **Not sound, and it UNDER-reports.** Any `pin_native_root` in the body clears
  the whole body, so a native that pins one reference and not another reads
  clean. Scopes are split at closure boundaries by regex, not parsed. A clean
  result here is not evidence of absence.
* **Not a ranking.** The table is alphabetical, not by risk.

The first cut produced 1074 candidates and was useless — registrar bodies were
being treated as one scope so bindings in one closure read as live across a call
in another, and tail-call `return ctx.invoke_*(...)` counted as having code
after it. Those two filters plus dropping test bodies took it to 32. Anyone
re-running this should expect to spend the effort on the filters, not the search.

## How to check one

The lever from the fixed record is the cheap discriminator, because it makes the
collector decline to move without changing anything else:

```bash
# fails with, passes without => an unpinned reference went stale
CRATONVM_MOVING_YOUNG_NO_JIT=1 cratonvm … <workload exercising the native>
```

The fix, when one is confirmed, is the two-line idiom already used everywhere
else — `ctx.pin_native_root(obj)` before the call, `ctx.read_native_pin(h, obj)`
to re-derive after it.

## Two blind spots this search has, both with a measured instance behind them

A confirmed defect of exactly this shape was found and fixed on 2026-08-24 —
`collect_collection_elements_or_real` dispatched `toArray()` on a receiver that
G1 had evacuated and recycled during the `size()` call one line above, and it
surfaced as the canonical `java.lang.Object` face:

```
NoSuchMethodError: 'java.lang.Object[] java.lang.Object.toArray()'
    from new ArrayList<>(map.keySet())
```

**It is not in the 32.** Two independent reasons, and each is worth a line here
because each is cheap to close:

1. **The search covers `native-builtins/src` only.** That defect is in
   `native-collections/src/lib.rs`, which holds its own several-thousand
   `ctx.invoke_*` sites and the whole collection-copy family. Re-running the
   same script over that crate is free.
2. **An unpinned PARAMETER is invisible to the heuristic.** The rule is "an
   object-typed local *bound* before a GC-capable call"; `coll` there is a
   function parameter, never re-bound, used across two `invoke_virtual` calls.
   A parameter is exactly as unrooted as a local and is arguably the more
   common shape, because a helper that takes a receiver and drives its Java is
   the normal way this code is factored.

A narrower complementary heuristic caught it in one pass and is worth running
alongside this one: **two or more `ctx.invoke_*` calls on the SAME receiver
identifier within one function, with no `read_native_pin` between them.** Over
`native-collections/src/lib.rs` that yields 13 candidates rather than hundreds,
because it does not care whether the body pins anything — only whether the
receiver is re-derived between the calls that can move it.

One caution learned from fixing that instance, on this page's own subject:
`report_reclaimed_receiver` said nothing throughout. It asks the FREE LIST, and
a whole evacuated G1 region is not a free-list block. So a quiet reclaim guard
does not clear a candidate off this list.

## Reproducing the audit

The script is `scratchpad/pinaudit.py` in the session that produced this page;
it is ~60 lines of regex over `native-builtins/src` and is not checked in,
deliberately — a heuristic with this false-positive rate should not look like a
gate. If this class is worth hardening properly it wants a real dataflow lint
over the 2058 `ctx.invoke_*` sites, with an allowlist for the two legitimate
cases in the table above.
