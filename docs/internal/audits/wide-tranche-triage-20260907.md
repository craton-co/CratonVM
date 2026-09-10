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

## The transitive tranche, triaged 2026-09-07

443 rows called "transitive" is not 443 decisions. A transitive row is a claim
about a CALLEE, and callees repeat — so the population reduces to a handful of
questions, each of the form "does this function actually allocate or re-enter
Java on a reachable path".

### It is not one bucket — it is a depth distribution

| depth the callee was marked at | rows |
|---|---|
| 0 — the callee's own body matches `ALLOC0` | **296** |
| 1 | 126 |
| 2 | 65 |
| 3 | 56 |
| 4–6 | 20 |

Depth 0 is not a guess: the statement calls a function that itself allocates.
Those 296 are as strong as a direct hit and were never the weak half. The
weakness the word "transitive" implies belongs to the ~140 rows at depth 2 and
beyond.

### Clustering by ROOT, not by immediate callee

`watch_base`, `aio_base` and `afc_base` are three one-line wrappers over one
function. Walking each row down to the depth-0 function it reaches — and to the
`ALLOC0` token inside it — gives the real decision points:

| rows | root | via |
|---|---|---|
| 65 | `drop` | `end_blocking_region` |
| 45 | `base_for_class` | `ensure_class_initialized` |
| 42 | `try_alloc_concurrent_synthetic` | `ensure_class_initialized` |
| 28 | `foreign_nio_delegate` | `invoke_virtual_bytecode_only` |
| 16 | `lk_member_access_flags`, `uri_has_synthetic_layout` | `declared_fields` |
| 14 | `collect_entries_any` | `invoke` |
| 31 | the four `make_*_stream` / `make_collector` | `try_alloc_synthetic` |

### Two of those roots were wrong, and both are now fixed

**`drop` — 65 rows from a NAME COLLISION.** `allocating()` keys the call graph
on the bare identifier before `(`, so all **forty** `fn drop(&mut self)` bodies
in these crates collapse into one node — and one of them, the TLS guard in
`t27_tls.rs`, calls `end_blocking_region`. That made the node `drop` a depth-0
allocator, and every `drop(guard)` / `drop(map)` / `drop(registry)` in the tree
inherited it. Sampled, the statements are mutex guards and hash maps, not the
blocking guard.

Excluding these names is sound and not merely convenient: a blocking region's
hazard is the window BETWEEN `begin_blocking_region` and `end_blocking_region`,
and the audit matches both tokens wherever they appear directly; the `drop` that
closes the guard is the end of a window it has already reported.
`UNRESOLVABLE_BY_NAME` holds the trait and std method names that cannot resolve
to one function. `get`, `new`, `build`, `finish` and `call` are deliberately NOT
in it — they collide too, but in these crates they are also the names of real
allocating helpers, and dropping them would trade a false positive for a false
negative.

**`declared_fields` and its three neighbours — 16 rows.** All four
(`declared_fields`, `declared_methods`, `class_annotations`,
`record_components`) are `&self` methods on `vm_exec` that take the
class-manager read lock and build a Rust `Vec` of metadata. No Java object, no
bytecode. Removed from `ALLOC0` — the same class of error as
`capture_stack_trace` and `get_ascii_case_string_cached`, which is now four
getters-named-like-producers found in one file.

### The big roots are REAL, and that is the finding

`base_for_class` reaches `ensure_class_initialized`, which runs `<clinit>` —
arbitrary bytecode. Every private-slot accessor in `native-io` is built on it
(`afc_get`, `aio_get`, `ws_get`, and their `_set` twins all call a `*_base`
wrapper), so **a plain-looking private field read in these crates is a GC
point**. That is 45 rows here and an architectural fact worth knowing
independently of this audit: it also means a cheaper `base_for_class` — one
that resolves an already-loaded class without initializing it — would remove a
real hazard from a large family at once.

> **Closed 2026-09-07: 45 rows → 4.** See
> [The `base_for_class` root, closed](#the-base_for_class-root-closed) below.
> The cheaper-`base_for_class` idea was only half of it — the accessor side was
> throwing away a class id it already held.

`try_alloc_concurrent_synthetic` (42), the `foreign_*_delegate` family (59
across four spellings) and the `make_*` stream constructors (31) are all
genuinely allocating or genuinely re-entering Java. Those rows stand.

### Effect

| tranche | base | `--opt` |
|---|---|---|
| `native-builtins/src` | 548 → 526 | 718 → 685 |
| `native-builtins/src/phases_late` | 73 → 70 | 84 → 81 |
| `native-collections/src` | 83 → 82 | 228 → 227 |
| `native-io/src`, `native-api/src` | unchanged | unchanged |

Base rows move again, and again they were spot-checked rather than assumed:
`alloc_instance_var_handle`'s window was `vh_has_synthetic_layout`, which is
GC-capable only through `declared_fields`. Correct drop.

### One more real site, found while triaging

`native_loader_load_module` — the sibling the function fixed earlier delegates
into — builds its receiver in a `_` arm that ALLOCATES and does not return, then
reads the name argument out of the pre-call `args`. `this` was already pinned
for a different window; the name was not. Fixed the same way.

## The `base_for_class` root, closed

45 rows → **4**, transitive total 381 → 342.

### The objection that had to be cleared first

`base_for_class`'s own doc comment recorded that a `class_id_by_name` sibling
**had already been written and removed**, because an accessor and its allocator
that disagree about the base is exactly the two-layouts-on-one-class condition
`appended_slots` exists to prevent. So "just look it up by name first" was not
an untried idea; it was a rejected one.

What was wrong with the sibling is that it answered **only** from
`class_id_by_name`, so on a miss it returned 0 where the allocator returned the
real count. The arm added here **falls through** to `ensure_class_initialized`
on a miss. The two can therefore differ only when the lookup answers `Some` —
and `Some` is precisely the case where they cannot: the VM implements it as
`find_unique_class_by_name`, which fails **closed** on a name several loaders
define (`None`, never one of the candidates — the whole reason
`classify_class_name` exists is that a plain `None` means *absent OR
ambiguous*). `Some(cid)` says the name resolves to exactly one class, which is
the one `ensure_class_initialized` would have returned. An ambiguous name takes
the old path unchanged.

The count never needed `<clinit>` to be right: `num_total_fields` is computed by
`compute_field_layout` at DEFINE time, and `class_manager` asserts that even
`redefine_class` leaves it alone. Skipping initialisation changes *when
`<clinit>` runs*, not what the base is.

### The bigger half was on the accessor side

`base_for_object` held the receiver's class id, threw it away, read the class
NAME back out of it, and handed that name to `base_for_class` to resolve a
second time. That round-trip is what actually cost the `<clinit>` on ordinary
private field reads — and on an ambiguous name it could resolve to a *different*
class than the receiver's and index the private map off that class's field
count.

It now asks a new `base_for_class_id(&dyn NativeContext, ClassId)` with the id
it already has. `&dyn`, not `&mut dyn`: every method it calls is `&self`, so the
signature is a compile-time statement that no GC can run inside it, and a later
edit reaching for `ensure_class_initialized` fails to borrow rather than
silently reopening the door. Two hand-written copies of the same accessor —
`pipe.rs::channel_private_base` and `synthetic_file_channel::private_base` — now
forward to it as well.

### The four survivors are correct, and were checked

`native_mbb_is_loaded`, `native_mbb_load`, `native_mbb_force` and
`native_fc_unmap0` call `base_for_class` with a CONSTANT name while holding a
receiver, so they still name a statically GC-capable function. Converting them
to `base_for_object` was considered and **rejected**:
`alloc_mapped_byte_buffer`'s class-resolution-FAILED arm allocates
`base + MBB_PRIVATE_WIDTH` against the untyped sentinel, so the substitute
`cratonvm/synthetic/AnonymousObject$N` is a stub and a receiver-derived base
would answer 0 where the allocator used a non-zero `base`. The per-class design
documented at `native-io/src/lib.rs` is right for these four. The new
already-loaded arm covers them at run time regardless — the audit simply cannot
see through the fallback.

### The JCA copies — done, and TWO of the reasons for doing it were WRONG

They were deferred here with two claims attached: that they **ratchet** in
synthetic-JDK mode, and that converting them would take 21 audit rows with it.
Both were written from reading the code. Measured, **neither holds**, and that
record is worth more than the deferral was.

There are also FOUR of them, not three: `synthetic_base_offset` in
`signature.rs`, `key_factory.rs` and `kem.rs`, plus `base_offset` in
`key_agreement.rs`. A grep for the first name finds three.

**The ratchet did not reproduce.** The predicted mechanism was: base 0 →
allocator asks `try_alloc_concurrent_synthetic(name, 0 + width)` → the `Err` arm
calls `try_ensure_synthetic_class(name, width)`, which fabricates a class
*declaring `width` fields* → the next `ensure_class_initialized` succeeds and
reads `width` back as the new base. A probe computing both the current answer
and the `base_for_class` answer on every call says otherwise:

    [JCABASE] first java/security/KeyPairGenerator: old=0 new=0
    [JCABASE] first java/security/KeyFactory:       old=0 new=0
    [JCABASE] first javax/crypto/KeyAgreement:      old=0 new=0

No value ever changed. These JDK classes are **pre-stubbed** in synthetic mode,
so `ensure_class_initialized` SUCCEEDS, `class_num_total_fields` answers 0, and
`alloc_object` never alters a class's declared count — the
fabricate-at-the-requested-width branch that would ratchet is never taken. The
header's claim is a fair reading of the code and is not reproducible for these
four classes.

**The 21 rows do not drop.** Checked rather than asserted: the transitive total
is **342 before and 342 after**. The rows move from `synthetic_base_offset` to
`base_for_class` (4 → 26), because `base_for_class` still contains
`ensure_class_initialized` in its cold fallback and the tool cannot see that the
warm path skips it. That follows from how `allocating()` builds its graph and
should have been predicted before the number was offered.

**What the conversion is actually worth**, both headline claims gone: it removes
a real `<clinit>` door from every JCA private-slot read — the same hazard class
as the `native-io` family above — and collapses four private re-implementations
into forwarders to the one helper, which `appended_slots`' header asks for by
name. The stub arm comes along defensively even though nothing here could make
it matter.

**Behaviour-preserving, measured.** The probe found the two shapes identical on
every class either mode reached — real-JDK `KeyPairGenerator` 2, `Signature` 4,
`KeyFactory` 5, `KeyAgreement` 6, `KEM` 4; synthetic-JDK all 0 — with no value
moving between calls. `test_classes/jca/JcaSlotFamilies.java` (new) signs and
verifies with ECDSA rather than introspecting, because most JCA engine state is
mirrored into side tables the accessors consult FIRST, so a fixture built on
`getAlgorithm()` passes straight through a wrong slot read. `SIG_OFF_KEYOBJ` is
the one slot with no side table behind it. A tampered-message negative control
keeps a constant `true` from satisfying it, and it passes on HotSpot 25.0.3
first.

Real-JDK, 5 rounds, `failures=0 skipped=0` on Generational, ZGC and G1, before
and after. Synthetic-JDK unchanged before and after: 4 `NoSuchMethodError`, 0
FAILs.

**A fixture caveat worth keeping.** The ECDSA round trip does NOT exercise
`SIG_OFF_KEYOBJ` in synthetic mode — that slot is populated only on the
`route_ec_to_real` SunEC drive path, which needs real EC classes that mode does
not have. `signature()` ran cleanly there for that reason, not because the slots
were proven good, which is why the base was measured directly rather than
inferred from the round trip.

**Found in passing, unrelated and unowned:** synthetic-JDK mode has no stub for
`java.security.spec.X509EncodedKeySpec` or `sun.security.ec.ECDHKeyAgreement`
(both `NoSuchMethodError`, "class not found on any classpath entry"). That is
the feature-gate rot `zgc-production-implementation-plan.md` R4 warns about, and
it belongs with the roadmap's open `P4-B — run --synthetic-jdk MODE`.

### Measurement

The one thing this change can get wrong is answering a **different number** than
the round-trip it replaces, so that is what was measured, not the aggregate
output. A temporary probe computed both answers on every real call and reported
each divergence plus a census every 512 looks — a zero with no looks is a claim
about the probe, not about the change.

`test_classes/gc/PrivateSlotFamilies.java` (new) drives all six families whose
base moved — FileChannel, Pipe, FileStore, DirectoryStream,
AsynchronousFileChannel, WatchService — with a background-allocation churn
between each allocation and each read, and asserts on values READ BACK OUT of
private slots, so a base off by one is a wrong answer rather than a crash. It
passes on HotSpot 25.0.3 first, as a check that the assertions are true of a
reference JVM and not merely of this one.

| arm | looks | mismatches | failures |
|---|---|---|---|
| ZGC (default) | 4608 | 0 | 0 |
| Generational | 4608 | 0 | 0 |
| G1 | 4608 | 0 | 0 |
| Generational, `CRATONVM_DISABLE_JIT=1` | 4608 | 0 | 0 |
| Generational, 200 rounds | 9728 | 0 | 0 |

**`NioChannelChurn` produced ZERO looks** — it opens through the real
`sun/nio/ch/FileChannelImpl`, which never reaches these accessors. Had it been
the only fixture, its clean run would have been a vacuous zero. That is why
`PrivateSlotFamilies` exists.

Controls, both 3/3 and both already green on dev: `GpuResidencyGc 0 1024 800`
and `NioChannelChurn 300 200 4` (`ok=300 bad=0`), Generational. Unit tests:
352 `native-api`, 4204 `native-builtins`, 528 `native-io`, including the
source-scanning `layout_alias_coverage` gate.

The new `base_for_class` test is a POSITIVE CONTROL, not just an assertion on a
number: `MockNativeContext::ensure_class_initialized` answers `ClassId::new(0)`
for every name — an id it never declares — so the fallback path can only return
0 there. Deleting the `class_id_by_name` arm turns the assertion from 3 into 0,
and that ablation was run. `class_num_total_fields` had to be added to the mock
for this: the trait default is a flat `0`, which collapsed every layout the mock
can model onto one answer, so no test could previously tell a base that was
computed from a base that was never reached.

## Still noisy

The four `native_stream_*` rows this page listed as surviving the returning-arm
rule are cleared by the `if`-level rule above, which reaches them through brace
depth rather than through the arm's text. What remains noisy is the TRANSITIVE
tranche — a callee reachable to an allocator within `--depth`, where deciding
the row means reading the callee. That is 443 of the original 547 and no rule
will thin it; it is a reading job.
