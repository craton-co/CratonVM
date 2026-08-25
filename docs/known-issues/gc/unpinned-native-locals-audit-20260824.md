# Eight natives fixed, and the search that found them has a blind spot worth 34 more

## Status

**OPEN.** Eight instances fixed 2026-08-24; the audit that found them was then
shown to miss an entire shape, and re-running it with that shape included yields
**34 further candidates in the same crate**. See "The blind spot is real" below.
This page moved to `docs/internal` as resolved for about an hour and moved back;
the fixes below stand, the SEARCH does not.

**What is fixed:** Audited `native-builtins` for the shape that cost a
week in
`bug-generational-ntru-unpinned-jit-reference-20260821-FIXED.md`, found eight
instances, fixed all eight with the idiom already used ~4900 times in this tree.
`cargo test -p cratonvm-native-builtins`: **4147 passed, 0 failed.**

## The shape

> a native holds a raw `ObjectRef` / `Value::Object` in a Rust local across a
> call that can allocate, and dereferences it afterwards.

The caller's operand slot is a root and gets remapped; the native's **copy** is
not and does not. A collection inside that window leaves the copy stale, and the
next read sees an all-zero header — which the class manager names
`java.lang.Object`. That is the whole of `d != java.lang.Object`.

## What was fixed

| file | function | held across | wrote through |
|---|---|---|---|
| `phases_early.rs` | `native_bs_init` | word-array `new_array` | `set_field(this, words)` |
| `phases_early.rs` | `native_bs_init_nbits` | word-array `new_array` | `set_field(this, words)` |
| `phases_late/io_streams.rs` | `p58_pushback_in_close` | delegated Java `close()` | `set_field(this, 0/1)` |
| `phases_late/io_streams.rs` | `p66_pushback_reader_close` | delegated Java `close()` | `set_field(this, 1)` |
| `phases_late/zip_streams.rs` | `p58_gzip_in_init_desc` | payload `new_array` | `set_field(this, 0)` |
| `phases_late/concurrent.rs` | `native_p65_pbq_offer` | grow `new_array` | `this`, `arr`, **`elem`** |
| `http_url_connection.rs` | `huc_init` | `URL.toExternalForm()` | `this`, `url_obj` |
| `lucene_es.rs` | `native_es_knn_score_doc_query_init` | two `new_array`s | `score_docs`, `reader`, **a `Vec<ObjectRef>` of hits** |

The last one is strictly worse than the defect that prompted the audit:
`format_impl` held one array, this holds a *collection* of raw refs and stores
every one of them back afterwards.

The fix everywhere is the established idiom:

```rust
let pin = ctx.pin_native_root(obj);
… GC-capable call …
let obj = ctx.read_native_pin(pin, obj);   // re-derive; it may have moved
```

## How the list was narrowed, and why that matters

The raw heuristic — object-typed local bound before a GC-capable call and used
after, in a body with no `pin_native_root` — returned **1074** candidates and was
worthless. Four filters took it to eight:

| filter | remaining |
|---|---|
| raw | 1074 |
| split scopes at closure boundaries (registrar bodies were one scope) | |
| drop registrars, tail-call `return ctx.invoke_*`, and test bodies | 32 |
| require a real object HANDLE, not data extracted from one | |
| require the handle to be DEREFERENCED after the call | **8** |

The third filter is the one worth keeping. `huc_set_request_property`'s `key`
looked like a textbook candidate and is a Rust `String` from `read_string()` —
immune to any move, because it is not a reference at all. Without that filter a
reviewer hand-checks a pile of those.

## What this does NOT establish

* **No dynamic proof.** All eight are *structurally* identical to a defect that
  was proven; none was observed failing. The fixes are correct regardless — a
  pin across a call that can move the object is right whether or not a workload
  currently hits it — but this page should not be read as "eight live bugs
  found".
* **The audit UNDER-reports.** Any `pin_native_root` anywhere in a body clears
  the whole body, so a native that pins one reference and not another still
  reads clean. A clean re-run is not evidence of absence.
* **Two legitimate patterns look like violations** and must stay on any future
  allowlist: `unsafe_natives.rs` pins the AQS blocker for *retention only*
  across `park()` and never dereferences it, and `graalvm_compat.rs` pins
  `ImageSingletons` permanently by design.

## The blind spot is real, and it is mine

`ea5ed2704` (an independent session, same day) found a confirmed defect of this
exact shape that **this audit could not see**, and named two reasons:

1. **Scope.** This search covered `native-builtins/src` only. Their defect —
   `collect_collection_elements_or_real`, dispatching `toArray()` on a receiver
   G1 had evacuated during the `size()` one line above — is in
   `native-collections/src/lib.rs`.
2. **Parameters are invisible.** My rule was "an object-typed local *bound*
   before a GC-capable call". A function PARAMETER is never `let`-bound, so it
   never matched — and as they point out, a helper that takes a receiver and
   drives its Java is the *normal* way this code is factored, so the parameter
   case is probably the commoner one.

Their complementary rule is better and does not care about binding at all: **two
or more `ctx.invoke_*` on the SAME receiver identifier within one scope, with no
`read_native_pin` between them.**

Re-running that rule, with this page's closure-scoping filter applied:

```text
native-collections/src   0     (they fixed it; 1065 read_native_pin there now)
native-builtins/src     34     <- NOT covered by the eight fixes above
```

Tightest spans first, which is roughly risk order:

```text
span 1   io_streams.rs:853          register_p70_object_streams()  `stream`
span 1   properties_sidetable.rs:3303  native_properties_equals()  `entry`
span 1   nio_file.rs:6759           register_phase57_nio_file()    `out`
span 1   ssl_security.rs:760/824    register_p68_crypto_mac()      `spi`
span 2   logging_shims.rs:1289      native_printstream_close()     `sink`
span 2   phases_early.rs:5643       enum_declaring_class_from_object()  `elem`
span 4   jmx.rs:2585                try_delegate_to_real_provider() `iter_obj`
span 4   service_loader.rs:3209     native_sl_spliterator()        `iter`
span 4   phases_late.rs:1369        collect_map_entries_as_strings() `entry`
```

Several are the iterator shape — `hasNext()` then `next()` on a receiver held
across both — which is structurally what their confirmed defect was.

**These 34 are not triaged and not fixed.** They have not been through the two
filters that took the original 1074 to 8 (real object handle; actually
dereferenced after), so the true count is lower. That triage is the remaining
work on this page.

One caution they record, which applies to anything on this list:
`report_reclaimed_receiver` stayed silent throughout their investigation. It
asks the FREE LIST, and an evacuated G1 region is not a free-list block — so a
quiet reclaim guard does not clear a candidate.

## If this is worth a gate

The heuristic is deliberately not checked in — something with a 1074-to-8
false-positive ratio should not look like a test. A real gate wants dataflow
over the 2058 `ctx.invoke_*` sites, with the two allowlisted patterns above.

The cheap per-candidate discriminator, if one is ever suspected dynamically, is
the lever from the parent record: `CRATONVM_MOVING_YOUNG_NO_JIT=1` makes the
collector decline to move without changing anything else, so fails-without /
passes-with is the signature.
