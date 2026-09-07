# Triage of the `--opt` (`wide`) tranche

| | |
|---|---|
| **Population** | 530 `wide` candidates across `native-builtins`, `native-collections`, `native-io`, `native-api` |
| **Read** | the 46 that are DIRECT + UNTAGGED — a real allocator or Java re-entry, not a transitive guess, not on a branch the audit already doubts |
| **Fixed here** | 10 functions — 2 in the first pass, 8 in the re-read |
| **Tool** | three false-positive CLASSES removed (closure definitions, returning match arms, returning `if` blocks) and four non-allocating tokens taken out of `ALLOC0` |
| **Not done** | the 484 transitive/branch-tagged rows, and 14 core rows left unread |

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

### Read AGAIN and fixed, 2026-09-07 (the 12 above, re-examined)

Reading each in full — rather than from the 320-character dump the first pass
used — moved four of the twelve out of the population and turned three others
into six, because the same defect had sibling copies.

**Four were NOT defects**, and two of them exposed wrong entries in `ALLOC0`:

| row | why not |
|---|---|
| `lang_string.rs` `string_case_impl(locale)` | `get_ascii_case_string_cached` is a pure lookup — `vm_exec` walks `thread.string_case_cache` and returns what it finds. It was in `ALLOC0`; **removed**, and its allocating sibling `create_ascii_case_string_cached` kept |
| `lang_stackwalker.rs` `native_fetch_stack_frames(args)` (first position) | `capture_stack_trace` -> `capture_current_stack_trace(&self)` reads frames into a Rust `Vec`; no Java object, no bytecode. `capture_stack_trace`, `capture_throwable_stack_trace` and `get_stack_trace` **removed** from `ALLOC0` |
| `native-builtins/src/lib.rs` `delegate_to_real_bytecode(args)` | same — its only "allocation" was that capture |
| `lang_class.rs` `native_class_for_name(args)` | the `ensure_class_initialized` is inside `if internal_name.starts_with('[') { … return … }`; the path that reads `args.get(2)` never runs it |
| `logging_shims.rs` `native_printwriter_write_string(args)` | its `invoke_virtual` is inside a block that returns; `printwriter_get_backing_writer` is two field reads |

That last pair is the returning-branch class again at `if` level rather than
match-arm level — the rule added earlier only blanks `=>` arms.

**Three had SIBLINGS with the identical shape**, found by reading rather than by
the audit (which reported them separately or not at all):

* `native_classloader_define_class1` -> also `define_class2` and `define_class0`
* `native_afc_read` -> also `native_afc_write`

**Fixed (8 functions):**

| site | stale across | pinned |
|---|---|---|
| `classloader.rs` `define_class_via_full` | `define_class_full` + `get_class_mirror` | `loader` AND `class_data` — the audit reported only `loader` |
| `jboss_module_loader.rs` `native_loader_load_module_by_identifier` | `ctx.invoke("getName")` + `create_string` | `args[0]`, before it is forwarded to the sibling native |
| `lang_stackwalker.rs` `native_fetch_stack_frames` | `populate_sfi` per frame | the receiver from `args`; `buffer` was already pinned, `args` was not |
| `lang_system.rs` `native_classloader_define_class1` / `2` / `0` | `preload_supertypes_via_loader` (loads classes) + `define_class_full` | the loader, at all three of its uses per function |
| `native-builtins/src/lib.rs` `native_formatter_init_locale` | `create_string` | receiver AND the locale argument |
| `phases_early.rs` `exchanger_do_exchange` | **`monitor_wait`** | receiver and the exchanged value, re-read at the top of each loop turn |
| `servlet.rs` `jython_new_module` | `create_string` | `dict`, before the constructor call |
| `native-io/src/lib.rs` `native_afc_read` / `native_afc_write` | `invoke_virtual("isReadOnly")`, which runs on the fall-through path too | the receiver |

`exchanger_do_exchange` is the widest window in the tranche and worth singling
out: every other site needs a collection to land inside a short call, and this
one BLOCKS in `monitor_wait` — a blocked thread is exactly where a peer's
collection runs.

All eight use `pin_native_root` / `read_native_pin` rather than a
`NativeHandleScope`, because the pins do not borrow `ctx` for a lifetime and so
need no restructuring of long function bodies. An unmatched pin on an error
path costs nothing: `safe_native_call_impl` truncates `native_pin_roots` when
the native returns.

**Verified**: the audit no longer reports any of the eight;
`GpuResidencyGc 0 1024 800` and `NioChannelChurn 300 200 4` are both 0/3 under
Generational with answers byte-identical to HotSpot;
`cargo test -p cratonvm-native-builtins --lib` 4204 passed and
`-p cratonvm-native-io --lib` 528 passed.

**Still reported, and correctly ignorable**: the three `define_class*` functions
keep one `args` row each, at an EARLIER statement — a `get_class_mirror` inside
an `if` block that returns. Same returning-branch class as above.

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

## The `if`-level returning branch, closed 2026-09-07

The two precision rules above handle a returning MATCH ARM by blanking it
inside one statement. The `if` form cannot be reached that way, because
`statements()` has already torn the block apart: the allocation and the
`return` are separate statements, with the `if` header a third.

    if let Value::Object(Some(out)) = ctx.get_field_by_name(this, "out") {
        let flushed = ctx.invoke_virtual(out, "flush", "()V", &[]);   <- allocates
        return Ok(None);                                              <- and LEAVES
    }
    if let Some(fd) = stream_fd(ctx, args) { … }                      <- "use after GC"

`dominates` recovers the nesting from brace depth and asks whether a block
CONTAINING the allocation closes before the use and ends in an unconditional
exit. Three things had to be right, and each was wrong first:

* **Depth at a statement's END, not its start.** A bare `}` still starts at the
  inner depth, so a walk looking for "the depth came back down" never saw a
  block close and concluded the use was inside it.
* **Only the blocks that CONTAIN the allocation.** The first version scanned
  everything between the two statements and swept in SIBLING blocks —
  `native_printstream_flush` allocates in a block that returns, but two later
  siblings do not, and their last statement is what the scan read.
* **Depth alone cannot compare siblings.** `native_class_for_name`'s allocation
  and its use are BOTH at depth 2, in two different blocks, one of which
  returns. A `dk <= dj` shortcut answered "reaches" for a path that does not
  exist.

A fourth thing was wrong in the plumbing rather than the rule: switching from
"latch the first GC-capable statement" to "pick the first candidate that
dominates" initially skipped any statement that both ALLOCATES and USES, which
dropped `stream_writeln` from the positive control. A use is now checked before
the statement is recorded as a candidate, so it is matched only against
candidates strictly before it — which is also what this file's "STATEMENTS, NOT
LINES" rule requires.

### It proves itself, both ways, every run

`dominates` DISMISSES, and this file's history says twice that a confident
dismissal is the dangerous kind. `assert_dominance_both_ways()` runs on every
invocation over four synthetic bodies — a block that returns, the same block
without the return, two sibling blocks, and a straight line — and aborts naming
the case. Forcing the function to always dismiss trips `if-falls-through`;
forcing it never to dismiss trips `if-returns`. A self-test that only
demonstrated the dismissal would pass while dismissing everything.

### Effect

| tranche | before | after |
|---|---|---|
| `native-builtins/src` | base 560, opt 754 | base 548, opt 719 |
| `native-builtins/src/phases_late` | base 74, opt 88 | base 73, opt 84 |
| `native-io/src` | base 4, opt 53 | base 4, opt 50 |
| `native-collections/src` | base 93, opt 243 | base 83, opt 228 |
| `native-api/src` | base 7, opt 10 | base 7, opt 10 |

The base tranche moves too — 24 rows — and those belong to another lane's
population, so they were spot-checked rather than assumed.
`native_arrays_to_string` and `native_stpe_schedule` both bind out of `args` in
a `match` whose other arms `create_string` and return; the allocation cannot
precede the use on any path that reaches it. Both drops are correct.

Both calibration controls still hold, and the eight functions fixed on
2026-09-07 remain unreported.

## Still noisy

The four `native_stream_*` rows this page listed as surviving the returning-arm
rule are cleared by the `if`-level rule above, which reaches them through brace
depth rather than through the arm's text. What remains noisy is the TRANSITIVE
tranche — a callee reachable to an allocator within `--depth`, where deciding
the row means reading the callee. That is 443 of the original 547 and no rule
will thin it; it is a reading job.
