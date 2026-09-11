# The BindableTests receiver was not rare: 20 more, and the array store is where this species actually lives

| | |
|---|---|
| **Status** | **SWEEP LANDED**, 2026-09-11. 20 defects fixed; the remaining population is baselined and ratcheted. |
| **Follows** | [the BindableTests residual](../springboot/bindabletests-assertj-objects-receiver-stale-across-clinit-20260911.md), which closed with "a pin is not a fact about a variable, it is a fact about a read" and no way to act on it |
| **Screen** | `scripts/stale-handle-across-alloc-audit.py` (new), baseline `scripts/baselines/stale-handle-across-alloc-sites.txt` |
| **Dynamic screen** | `[deadref-recv]` extended from `set_field` to `set_array_element`, `get_array_element` and `get_field` |

## What the predecessor page left open

It named the generalisation and admitted the gate could not see it. The
standing audit it inherited — 28 candidates in one file, screened by
`[deadref-pin]` — asks whether a value was DEAD WHEN PINNED.
`scripts/stale-receiver-audit.py`, the real gate, asks whether a HELPER takes a
receiver by value, allocates, and returns `()` so its caller is stuck with the
old address. Its baseline is empty and it is still correct.

Neither can see a function that allocates and then reuses its own local:

```rust
let arr = ctx.new_ref_array(cid, n);
for i in 0..n {
    let e = try_alloc_concurrent_synthetic(ctx, "…", 4)?;  // ← can collect
    ctx.set_array_element(arr, i, Value::Object(Some(e)));
    //                    ^^^ the address new_ref_array returned
}
```

No helper is involved, no pin is mishandled, and nothing crashes. The store
lands in the pre-move copy; the surviving array keeps `null` in that slot.

## The finding that changes the shape of the problem

The residual was a `set_field`. **The commonest form is a `set_array_element`**,
because the natural way to build a Java array in a native — allocate the array,
then allocate its elements — holds the array's address across every element's
allocation. Of the 20 defects fixed here, 17 are array receivers.

That also explains a gap in the instrument added with the residual:
`[deadref-recv]` was wired into `set_field` only, which is the *rarest* of the
three places this happens. It now covers `set_array_element`,
`get_array_element` and `get_field` as well. The read side matters for the same
reason in reverse: a read through a vacated receiver answers whatever the
pre-move copy held — usually the JVM default — and nothing about the answer says
where it came from. `every_receiver_taking_primitive_reports_a_vacated_receiver`
(gc) pins all three, catching the dead-address `debug_assert` that follows each
array access so the test asserts the SCREEN, not the completion of the access.

## What was fixed

Every one is the same edit: root the reference in a `NativeHandleScope` and read
it back through `scope.get(&h)` after anything that can collect, instead of
carrying the address the allocator returned.

| site | what was crossing the allocation |
|---|---|
| `lang_misc::fill_stack_trace_element` | the element, across THREE `create_string`s and a class mirror |
| `lang_misc::native_throwable_get_stack_trace_array` | the array AND the element, once per frame |
| `lang_system::build_stack_trace_element_array` | same, for `Thread.getStackTrace()`/`dumpThreads()` |
| `lang_system::native_system_getenv_all` (both layout arms) | the bucket array, the map, both strings and the node, per environment variable |
| `lang_string::native_string_lines` | every `String` made in the loop went stale under the ones made after it, then all were stored into an array allocated later still |
| `phases_early` `Collections.singletonList/Set/Map` | the element argument across `<clinit>`, and the collection across the map/array/node allocations |
| `phases_early` `Properties.<init>` / `setProperty` | the receiver across the backing array, and receiver + array + both arguments across the growth copy |
| `phases_late` `ClassLoader.getResources` | the array across a `URL` and its string, per entry |
| `phases_late` `BigInteger(int, byte[])` | the receiver across the magnitude array and the decimal string |
| `phases_late/concurrent` `PriorityBlockingQueue.<init>` / `toArray` | the receiver, and the source array across the copy's allocation |
| `phases_late/reflect_invoke` `Module.getModules` | the array across a `Module` and a `String` per module, then the `HashSet` |
| `phases_late/text_intl` `ChoiceFormat(String)` | the receiver and both arrays across a box and a string per segment |
| `phases_late/xml_json` `build_json_tree_node_depth` | the node and its two arrays across a whole recursive subtree per pair |
| `t3_impl` StAX event/stream readers | the event array across an event and up to two strings per event |
| `t3_impl` JNDI `list` | the source array across the result's allocation |
| `t3_impl` charset map | the map and both arrays across four allocations per charset |
| `util_concurrent_ext` `ConcurrentSkipListMap.<init>`/`put` | the receiver, both arrays and both arguments — `compareTo` runs Java on EVERY probe of the search loop |
| `inet_address::lookup_all_host_addr_impl` | the array across one mirror per address |
| `jmx` `getMemoryManagers0`, `unregisterMBean` | the array across the manager; three arrays and the receiver across two more allocations |
| `locale_bootstrap::get_available_locales` | the array and the locale across three strings and a constructor call |
| `lucene_es::native_es_knn_score_doc_query_init` | `docs_arr` across the very next `new_array` |
| `jboss_msc::alloc_java_service_name` | the object across two strings and a recursive parent chain |
| `xnio_worker::native_worker_get_io_threads` | the array and the receiver across one mirror per thread |

`Throwable.getStackTrace()` and `Thread.getStackTrace()` are the two that should
worry a reader most: they are on every exception path, and a lost element there
is a blank or null frame in a stack trace — which is read as a VM quirk, not as
a heap defect, so it can survive indefinitely.

## The screen

`scripts/stale-handle-across-alloc-audit.py`, a sibling of
`stale-receiver-audit.py` and deliberately not a replacement for it. Same
conventions: `--detail`, `--update`, `--selftest`, baseline in
`scripts/baselines/`, exit `1` when the population grows.

It reports **283 sites in 202 functions** after this sweep. That number is a
RATCHET, not a target, and the page has to be honest about why it is not a
defect count:

* the receiver may be an old-generation singleton, which never moves;
* the "allocating" call may be on a path that cannot allocate in practice;
* the scanner does not follow calls, so a helper that allocates two frames down
  is invisible unless its own name is in `HAZARD` — the count is a floor as
  well as an overcount.

The first version of the scanner said 573, and 290 of those were one bug in the
scanner rather than in the tree: a `register_*` function is a hundred
`|ctx, args| { … }` closures that share nothing, and reading them as one flat
line pairs an allocation in one closure with a use in another. Closures are
scanned as their own bodies now. That correction is worth recording because the
uncorrected number would have made the next tranche look twice its real size,
and a triage queue nobody believes is a queue nobody works.

What the number IS good for is the derivative. A new native that reuses a local
across an allocation now trips a gate instead of waiting for a GC-stress run on
a workload nobody has written yet.

The hazard mix across those 283 sites says where the next tranche is:
`create_string` 77, `invoke*` 74, `new_array` 43, `try_alloc*` 30,
`new_ref_array` 12. The `invoke*` ones are the most interesting and the least
mechanical — a native that calls back into Java and then keeps using anything it
read beforehand, which is the `apps_h2`/`x509_manager`/`util_concurrent_ext`
cluster — and they are the natural next unit of work. By file, the queue's head
is `lang_invoke.rs` (25), `phases_late/nio_file.rs` (18),
`phases_late/ssl_security.rs` (14), `jmx.rs` (10).

The selftest is not decoration. Its sibling's docstring records three hazard
tokens that matched NOTHING in the tree for months, which reads exactly like a
clean tree; every token in `HAZARD` here has an example asserted on every run,
plus one canonical positive and one canonical FIXED form, so a scanner that
stops detecting fails the job instead of reporting zero.

## Verification

* `cargo test -p cratonvm-gc --lib` — 1909 passed (includes the two receiver-arm
  tests), `-p cratonvm-native-builtins --lib` — 4228 passed.
* `BindableTests`, Linux x86-64, `--XX:UseGc Generational`, `-Parallel 1`:
  27/27 at `CRATONVM_DBG_GC_STRESS` 65536, 131072, 262144, 262144 `--nojit`,
  393216, 524288 and default — the matrix the predecessor page established, on
  the tree with all 20 fixes.
* A/B on the 15-class spring-boot smoke batch, same host, same runner, the
  binary before this sweep (`mrg1`) against the binary after it (`arr1`):

  | | default GC | `CRATONVM_DBG_GC_STRESS=262144` |
  |---|---|---|
  | before | 15 PASS | 9 PASS · 1 FAIL · 5 CRASH |
  | after | 15 PASS | 9 PASS · 1 FAIL · 5 CRASH |

  Identical, class for class, including WHICH six fail. No regression, and no
  claim that the sweep fixed any of them.

## The six the A/B exposes, which are the next unit

That stress column is the useful part of the A/B. Six of fifteen ordinary
spring-boot classes do not survive `CRATONVM_DBG_GC_STRESS=262144` on either
binary, and they are reproducible in under thirty seconds each — which is
better evidence than anything the static screen can produce:

| class | symptom |
|---|---|
| `BannerTests` | `NullPointerException: Proxy$Dispatch.invokeProxy: proxy InvocationHandler is null` — a proxy's handler field reads null |
| `ApplicationEnvironmentTests` | CRASH |
| `ApplicationInfoPropertySourceTests` | CRASH |
| `ApplicationPropertiesTests` | CRASH |
| `ResourceBannerTests` | CRASH |
| `ShutdownEndpointTests` | CRASH |

`BannerTests`' shape is this species' signature: a field that is null when the
object it belongs to was built while something else was allocating. Whether all
six are this family is exactly what the next unit has to establish rather than
assume — and with a reproducer per class, `[deadref-recv]`'s three new arms are
finally being pointed at a workload that fails.

The honest limit, restated from the predecessor page because it did not change:
these fixes are not each demonstrated by a failing test. The species produces a
plausible wrong answer at one exact instant, so a green suite is weak evidence
either way. What the suite establishes is that the fixes did not break anything;
what the screen establishes is that the population cannot grow quietly.
