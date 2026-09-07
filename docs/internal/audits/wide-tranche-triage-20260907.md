# Triage of the `--opt` (`wide`) tranche

| | |
|---|---|
| **Population** | 530 `wide` candidates across `native-builtins`, `native-collections`, `native-io`, `native-api` |
| **Read** | the 46 that are DIRECT + UNTAGGED — a real allocator or Java re-entry, not a transitive guess, not on a branch the audit already doubts |
| **Fixed here** | 2 sites (3 rows) |
| **Tool** | two systematic false-positive classes found and removed |
| **Not done** | the other 484 rows |

## Why 46 and not 530

The tranche splits three ways, and the splits are not equal in value.

**443 of the original 547 were TRANSITIVE** — the statement counts as
GC-capable because a callee is reachable to an allocator within `--depth`, not
because it allocates itself. Those need the callee read before the site means
anything, and reading a callee is most of the cost of reading the site.

**64 carried `~`** — `branchy`, i.e. the audit already says the GC-capable
statement may not dominate the use.

What is left — a direct allocator or `invoke_*`, no branch caveat — is the
tranche where the grep has done all the work it can and a read decides. That is
46 rows, and this page is the read.

## Two false-positive classes, removed from the tool

Both were found by reading, and both are worth more than the individual sites
they cleared: they were inflating every future run.

### A closure DEFINITION is not an execution

```rust
let empty = |ctx: &mut dyn NativeContext| {
    let arr = ctx.new_ref_array(ClassId::new(0), 0);   // allocates WHEN CALLED
    ...
};
let this = match args.first() { … };                   // reported as "use after GC"
```

Binding the closure allocates nothing. `class_annotations_by_type_impl` and
`native_method_get_annotations_by_type` both open with one and both were
reported. `gc_capable` now returns false for a `let NAME = |…|` binding.
An immediately-invoked closure (`(|| { … })()`) and one handed to a caller that
runs it (`catch_unwind(AssertUnwindSafe(|| …))`) are not `let` bindings and
still count — correctly, because both run before the next statement.

### An allocation on a RETURNING arm does not dominate what follows

```rust
let this = match args.first() {
    Some(Value::Object(Some(r))) => *r,
    _ => { let opt = try_alloc_synthetic(ctx, …)?;      // allocates
           return Ok(Some(Value::Object(Some(opt)))); } // and LEAVES
};
let operator = match args.get(1) { … };                 // "use after GC"
```

On every path that reaches the next statement, nothing was allocated. `branchy`
cannot see this — it fires on a statement STARTING with `return` or a bare
`=>`, and this one starts with `let`. `alloc_only_on_returning_arm` blanks the
arms that return and asks whether what remains is still GC-capable; it clears
the statement only when none of the surviving arms is, which is the safe
direction.

Together: 547 → 530, and the direct+untagged core got materially cleaner.

## The 46, read

### Fixed here (3 rows, 2 sites)

Both are the same textbook shape — grow an array, then store a reference
parameter that has been sitting in a Rust local across the allocation — and
both had MORE than the reported reference at risk: the old array being copied
from and the receiver being published into are stale too.

| site | stale across | what was at risk |
|---|---|---|
| `native-builtins/src/lib.rs` `m18_lbq_add_internal(elem)` | `ctx.new_array` | `elem`, the source array, the receiver |
| `native-builtins/src/t3_impl.rs` `jndi_put_binding(name, value)` | TWO `ctx.new_array` calls | `name`, `value`, both source arrays, `bindings` |

Converted to a `NativeHandleScope` over the grow path, with every reference
re-read at its use and the receiver carried back out for the count store that
runs after the scope closes. The audit no longer reports either.

### Read and judged NOT a defect (17 rows)

| site | why not |
|---|---|
| `logging_shims.rs` `native_printstream_flush` | its only Java re-entry is inside an `if let` block that `return`s; the `stream_fd(ctx, args)` path never sees it |
| `native-collections` `native_stream_to_array_gen`, `native_stream_reduce_optional`, `native_stream_min`, `native_stream_max` | allocation on a returning match arm (the class above; these four survive because the arm's text defeats the blanking — see "Still noisy") |
| `zip_streams.rs` `native_inflater_input_stream_init` | same shape |
| `lang_class.rs` `native_class_get_nest_members` | same shape |
| `test_frameworks.rs` `assertj_objects_equal(left, right)` | the "GC" statement and the "use" are the SAME `match` expression — the use is one of its arms |
| `native-collections` `make_summary_statistics(sum, min, max)` | `Value::Long`/`Double`. A scalar copied out of a `Value` has nothing to dereference |
| `native-io/src/process.rs` `alloc_process_handle(pid)` | a pid — scalar, same reason |
| `native-builtins/src/lib.rs` `pd_gather_custom(elems)` | the use is `elems.is_empty()`, a length test |
| `lang_invoke.rs` `collect_trailing_varargs(params)` | `ctx.declared_methods` is a metadata read, not an allocation |

### Read and judged REAL, NOT fixed here (12 rows)

Each is a reference parameter or an `args` element dereferenced after a genuine
allocation or `invoke_*`, with no intervening return. Left for the sweep
because each needs its own conversion and none has a reproducer yet:

* `classloader.rs` `define_class_via_full(loader)` — across `define_class_full`
* `jboss_module_loader.rs` `native_loader_load_module_by_identifier(args)` — `args[0]` into a fresh arg vector after `ctx.invoke`
* `lang_class.rs` `native_class_for_name(args)` — across `ensure_class_initialized`
* `lang_stackwalker.rs` `native_fetch_stack_frames(args)` — across `capture_stack_trace`
* `lang_system.rs` `native_classloader_define_class1(args)` — across `capture_stack_trace`
* `native-builtins/src/lib.rs` `delegate_to_real_bytecode(args)` — `extend_from_slice(args)` after `capture_stack_trace`, then invoked
* `native-builtins/src/lib.rs` `native_formatter_init_locale(args)` — across `create_string`
* `lang_string.rs` `string_case_impl(locale)` — across `get_ascii_case_string_cached`
* `logging_shims.rs` `native_printwriter_write_string(args)` — `args.get(1)` after `printwriter_get_backing_writer`
* `phases_early.rs` `exchanger_do_exchange(my_val)` — across `monitor_wait`, which BLOCKS, so a peer thread's collection is the likely one
* `servlet.rs` `jython_new_module(dict)` — across `create_string`
* `native-io/src/lib.rs` `native_afc_read(args)` — across `invoke_virtual(dst, "isReadOnly")`

### Not read (14 rows)

`build_module_spec_via_invoke`, `put_non_string_into_chm` (2),
`build_service_loader` (2), `jlrefa_new_parameter`,
`native_surefire_lookup_decoder_factory`, `javac_platform_class_file_object` (2),
`spring_class_utils_for_name_impl`, `socket_option_name`,
`native_opt_if_present_or_else`, `box_primitive_result`,
`tm_reverse_comparator`, `native_lbq_put_blocking`, `alloc_completed_future`,
`native_cslm_init_comparator`, `new_object_initialized`.

## Rate

Of the 32 rows actually read: **15 real, 17 not**. Slightly under half, against
34-of-46 for the `local` tranche in `a189643cc`. The difference is the one the
audit's own header predicts: a `wide` row is an argument, and
`safe_native_call_impl` PINS every argument, so the "zeroed in place" half of
the family cannot apply to it. Only RELOCATION can, which is a narrower hazard
and needs a moving young collection at one instant.

## Still noisy, and worth a rule

Four `native_stream_*` rows are the returning-arm class and survive it, because
the arm's own text contains braces the blanking regex will not cross. A
brace-matching arm splitter would clear them; a regex will not. That is the
next precision improvement, and it is worth roughly a tenth of this core.
