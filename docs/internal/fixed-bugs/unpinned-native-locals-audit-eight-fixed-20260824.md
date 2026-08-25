# ✅ RESOLVED — eight natives held an unpinned reference across a GC-capable call

## Status

**RESOLVED 2026-08-24.** Audited `native-builtins` for the shape that cost a
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

## If this is worth a gate

The heuristic is deliberately not checked in — something with a 1074-to-8
false-positive ratio should not look like a test. A real gate wants dataflow
over the 2058 `ctx.invoke_*` sites, with the two allowlisted patterns above.

The cheap per-candidate discriminator, if one is ever suspected dynamically, is
the lever from the parent record: `CRATONVM_MOVING_YOUNG_NO_JIT=1` makes the
collector decline to move without changing anything else, so fails-without /
passes-with is the signature.
