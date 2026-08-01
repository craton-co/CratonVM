# `vm/src/jit/` cache keying — census, fixes, and what remains

**Status: 🟡 PARTIALLY FIXED 2026-08-01.** Every `ClassId`-keyed and
call-site-keyed memo in `vm/src/jit/` now carries a VM key, and the one
thread-local that caches raw heap addresses now carries a heap key. One
genuinely dangerous item — a thread-local `ObjectRef` with **no GC root
provider at all** — is documented with a recipe but not fixed, because the fix
lands outside this directory.

This is the follow-on the round-1 sweep asked for.
[`vm-process-global-state.md`](vm-process-global-state.md) censused `vm/src/`
and explicitly excluded `vm/src/jit/` with the note that it "contains several
`ClassId`-keyed inline caches with no VM key, which would be miscompile-grade".
That note was correct. **Read the round-1 doc first** — the three categories
(*benign* / *per-VM leak* / *dangerous*), the `vm_identity` and `heap_id`
keying conventions, and the "both halves or neither" GC-root rule are all
defined there and are not restated here.

## Why this directory is the worst place for the bug class

Everywhere else, a cross-VM cache hit leaks state. Here it feeds the code
generator or the calling convention:

* a `DISPATCH_CACHE` hit hands compiled code a **raw entry pointer into another
  VM's code cache**, and the helper immediately `CALL`s it with *this* VM's
  `vm_ptr`. The callee's baked constants — class ids, static-field slots,
  direct-call targets — all belong to the other VM.
* an `OBJECT_NATIVE_DISPATCH_CACHE` hit says "receiver class id *X* is exactly
  `java/util/HashMap`, dispatch this callback". If *X* names a different class
  in this VM, the `HashMap` native runs against a foreign object layout.
* a `class_init_memo` hit says "class id *X* has completed `<clinit>`", which
  makes a compiled `getstatic` skip the JVMS §5.5 check and read the
  zero-initialized placeholder.

None of these degrade to a slow path. They produce wrong machine behaviour that
surfaces as heap corruption and gets attributed to the GC.

## Threat model — how a thread reaches two VMs

Most of the items below are `thread_local!`, which is *not* the same as
per-VM. A thread sees two VMs when:

* an OS thread attaches to a second `JavaVM` via JNI `AttachCurrentThread`;
* a test/harness thread is reused across sequentially constructed `SharedVm`s
  (the common case in this repo's own unit-test binary);
* a VM is torn down and another is created — enough on its own for any key
  built from a recyclable *address*.

That last point is why every fix below keys on `SharedVm::vm_identity`
(`vm/src/vm/vm_init.rs:9` — `NEXT_VM_IDENTITY: AtomicUsize = AtomicUsize::new(1)`,
only ever `fetch_add`ed, so never reused and never `0`) rather than on
`vm as *const SharedVm as usize`.

## Census

`file:line` is post-fix. "Unique across VMs?" describes the key **before** this
change.

### `vm/src/jit/helpers.rs`

| item | key | unique across VMs? | wrong-hit consequence | holds heap pointer? | action |
| --- | --- | --- | --- | --- | --- |
| `DISPATCH_CACHE` `:7217` | `JitInvokeInfo` ptr | **no** | CALLs another VM's compiled body | raw code ptr (pinned) | **FIXED** — `JitSiteKey` |
| `VIRTUAL_DISPATCH_CACHE` `:7239` | (info ptr, receiver `ClassId`) | **no** — both halves per-VM | as above, at a polymorphic site | raw code ptr (pinned) | **FIXED** — `JitSiteKey` |
| `VIRTUAL_TARGET_CACHE` `:1704` | (info ptr, receiver `ClassId`) | **no** | resolves the callee by another VM's class name | no | **FIXED** — `JitSiteKey` |
| `OBJECT_NATIVE_DISPATCH_CACHE` `:7221` | `JitInvokeInfo` ptr | **no** | runs `HashMap`/`Matcher`/`StringBuilder` natives on a foreign layout; miscounts another VM's native census | no | **FIXED** — `JitSiteKey` |
| `INTEGER_NATIVE_DISPATCH_CACHE` `:7224` | `JitInvokeInfo` ptr | **no** | applies VM A's JDK-only admission verdict and `NativeMethodId` in VM B | no | **FIXED** — `JitSiteKey` |
| `DISPATCH_COUNTER` `:7219`, `VIRTUAL_DISPATCH_COUNTER` `:7242` | as their caches | **no** | tiers a site up on another VM's hotness | no | **FIXED** — `JitSiteKey` (shares the key with its cache) |
| `class_init_memo` `:5256` | raw `ClassId` (process-global bitmap) | **no** | compiled `getstatic`/`putstatic` skips JVMS §5.5 `<clinit>` for a class that never ran it → reads `Int(0)` placeholder → NPE / silent wrong value | no | **FIXED** — owner latch |
| `system_class_memo` `:5355` | raw `ClassId` (process-global bitmaps) | **no** | suppresses the `System.out`/`err` bootstrap intercept in VM B (`println` no-ops on a null stream), or applies it to an unrelated class | no | **FIXED** — owner latch |
| `system_class_id` (deleted) | none — one process-global `AtomicU32` | **no** | first VM's `java/lang/System` id latched for the process | no | **DELETED** — dead code, zero callers |
| `JIT_TYPECHECK_TARGET_CACHE` `:5945` | (`&SharedVm` addr, name ptr, len) | address-recyclable | `checkcast`/`instanceof` resolves to another VM's target class | no | **HARDENED** — `vm_identity` |
| `JIT_SUBTYPE_POSITIVE_CACHE` `:5974` | (`&SharedVm` addr, child, parent) | address-recyclable | false-positive subtype ⇒ a cast that must throw CCE succeeds | no | **HARDENED** — `vm_identity` |
| `JIT_TYPECHECK_ANSWER_CACHE` `:6031` | (`&SharedVm` addr, name ptr, len, cid, lenient) | address-recyclable | as above, whole-answer | no | **HARDENED** — `vm_identity` |
| `INTEGER_WRAPPER_CLASS_CACHE` `:7230` | (`&SharedVm` addr, cid) | address-recyclable | TLAB-bumps a wrapper with the wrong class id / slot count | no | **HARDENED** — `vm_identity` |
| `MATCHER_CLASS_CACHE` `:7234` | (`&SharedVm` addr, cid) | address-recyclable | "exactly `Matcher`" answered for a foreign class | no | **HARDENED** — `vm_identity` |
| `HASHMAP_CLASS_CACHE` `:8851`, `CONCURRENT_HASHMAP_CLASS_CACHE` `:8846` | (`&SharedVm` addr, cid) | address-recyclable | thin direct helper runs against a non-`HashMap` receiver | no | **HARDENED** — `vm_identity` |
| `INTEGER_ALLOC_SLOTS` `:8636` | (`&SharedVm` addr, cid, slots) | address-recyclable | allocates a wrapper with another class's field count | no | **HARDENED** — `vm_identity` |
| `JIT_SIGNALS.exception` `:303` | thread-local, no key | n/a (per-thread) | **use-after-free / stale pointer**: a live `ObjectRef` with no `scan` and no `remap` | **yes, `ObjectRef`** | **OPEN** — recipe below |
| `JIT_SIGNALS` (`aioobe`/`arithmetic`/`npe`/`deopt`/`athrow_bci`) `:303` | thread-local | n/a | none — primitives, drained on the same return | no | benign |
| `CURRENT_JIT_CALLEE` `:259`, `JIT_THREAD` `:272` | thread-local | n/a | diagnostic string / this thread's own `JvmThread` ptr | no | benign |
| `DISPATCH_CACHE_SUPERSEDE_EPOCH` `:7279`, `DISPATCH_CACHE_JIT_GENERATION` `:7280` | thread-local epoch stamps | process-wide counters | over-flushes (conservative) | no | benign |
| `JIT_SELF_CALL_STACK_FLOOR` `:12601` | thread-local | n/a | this thread's own native stack bound | no | benign |
| `JDK_ONLY_HELPER_VIOLATIONS` `:6936`, `JDK_ONLY_FASTPATH_REFUSALS` | process-global report log | n/a | merges two VMs' violation reports (intended) | no | benign |
| `mic_prof`/`gs_prof` counters, all `OnceLock<bool>` env gates | none / env var | n/a | diagnostics; flags are process-wide by definition | no | benign |

### `vm/src/jit/conservative_roots.rs`

| item | key | unique across VMs? | wrong-hit consequence | holds heap pointer? | action |
| --- | --- | --- | --- | --- | --- |
| `JIT_SCAN_CACHE` `:1006` | (boundary gen, chain len, collection count) | **no heap key** | replays one heap's raw object addresses into another heap's root vector — the `oscache` failure mode aimed at the mark phase | **yes, `Vec<ObjectRef>`** | **FIXED** — `heap_id` added to the key |
| `JIT_ENTRY_CHAIN` `:176` | thread-local stack | n/a | this thread's own frame records (stack addrs + `Arc`-owned `CompiledMethod` ptrs) | no heap ptrs | benign |
| `JIT_BOUNDARY_GEN` `:1005`, `TOP_RBP` `:230`, `UNREG_JIT_VERIFIED_LO` `:216`, `CACHED_HIGH` `:554` | thread-local | n/a | this thread's own stack geometry | no | benign |
| `RANGE_SNAPSHOT` `:1229` | `jit_code_ranges_generation()` | process-wide, and so is the code arena | none | code addrs, not heap | benign |
| `GLOBAL_JIT_DEPTH` `:338` | none | process-wide **by design** — code pages are process-wide | none | no | benign |

### `vm/src/jit/code_cache_lifecycle.rs`, `alloc_class_cache.rs`, `skip_list.rs`, `disasm.rs`, `xt_root_scan.rs`

| item | key | unique across VMs? | wrong-hit consequence | holds heap pointer? | action |
| --- | --- | --- | --- | --- | --- |
| `PROCESS_LIFECYCLE` `code_cache_lifecycle.rs:1477` | `MethodId` = hash of the `(class, name, descriptor)` **name** triple | yes — content hash, like round-1's `padded_code_cache` | merges two VMs' version counts in a diagnostic; retirement quiescence is genuinely process-wide | code extents, not heap | benign — comment added |
| `JitAllocClassCache` `alloc_class_cache.rs` | `ClassId` | yes — **not a process global**; it is a field of `JitRealm` in `Arc<SharedVm>` | n/a | no | benign — comment added |
| `skip_list.rs` `ON` / `BISECT` / `ONLY` / `CACHE` (`:231`, `:559`, `:589`, `:2504`) | env vars, parsed once | process-wide by definition | none | no | benign |
| `disasm.rs` `CACHE` `:20` | `CRATONVM_DBG_JIT_DISASM` | as above | none | no | benign |
| `xt_root_scan.rs` `SLOTS`/`RANGES`/`ACTIVE`/`HELPER_MODE` (`:821`-`:826`) | OS **tid** (process-unique) and JIT code ranges (process-wide) | yes | none from keying | transient register/stack words, committed within one collection | benign keying; see the concurrency caveat below |
| `xt_root_scan.rs` `XT_*` counters, `enabled()`/`helper_window_scan_enabled()` `CACHE` | none / env | n/a | diagnostics, flags | no | benign |

## What was fixed

### 1. Every dispatch memo is keyed on `(vm_identity, info ptr)` — `helpers.rs`

New `pub(crate) type JitSiteKey = (usize, usize)` and `jit_site_key()`
(`helpers.rs:1662-1694`). `jit_invoke_dispatch` builds it once
(`helpers.rs:7737`) and every downstream map uses it unchanged, so the churn is
small: the seven maps' value types are untouched.

**The premise, verified rather than assumed.** A `JitInvokeInfo` pointer is not
a process-unique site identity, for two independent reasons:

* Six call sites dispatch through the address of a **process-global `static
  JitInvokeInfo`** — `INTEGER_VALUE_OF_INFO` (`:8512`), `INTEGER_INT_VALUE_INFO`,
  `HASHMAP_PUT_DIRECT_INFO`, `HASHMAP_GET_DIRECT_INFO`,
  `CONCURRENT_HASHMAP_GET_DIRECT_INFO`, `STRING_LATIN1_LOWER_DIRECT_INFO`
  (`helpers.rs:8747`, `:8807`, `:8816`, `:8825`, `:8835`) — passed to `jit_invoke_dispatch` as `info_ptr`
  (e.g. `helpers.rs:8799`, `:8968`, `:9096`, `:9242`). That address is *identical* in every VM. This is
  not a recycling hazard, it is a guaranteed collision.
* Every other info comes from `JitCache::invoke_info_arena`
  (`jit/src/lib.rs:8311`), a per-`JitCache` arena. `JitCache` is a by-value
  field of `JitRealm` (`vm/src/vm/realms/jit_realm.rs:19`/`:70`), so the arena is
  freed with its VM and a later VM's arena can reissue the address.

**What limited the blast radius before (and why it was not enough).**
`DISPATCH_CACHE` / `VIRTUAL_DISPATCH_CACHE` are flushed whenever
`cratonvm_jit::jit_cache_generation()` or `classloading::class_definition_epoch()`
moves (`helpers.rs:1735-1780`), and both counters are *process-global* atomics
(`jit/src/lib.rs:8317`, `classloading/src/class_manager.rs:1218`), so a second
VM's bootstrap over-flushes the first VM's memos — accidentally protective.
But `VIRTUAL_TARGET_CACHE` is flushed only on the class-definition epoch, and
`OBJECT_NATIVE_DISPATCH_CACHE` / `INTEGER_NATIVE_DISPATCH_CACHE` are flushed by
**nothing at all** — no `.clear()` on either exists anywhere in the file. Those
two were unconditionally exposed.

### 2. `class_init_memo` and `system_class_memo` are owned by exactly one VM — `helpers.rs`

Both are dense bitmaps indexed by a raw `ClassId`, both were unqualified
process globals, and both are consulted from `jit_getstatic`
(`helpers.rs:5498`, `:5526`, `:5556`).

`class_init_memo` is the more serious of the two. Its own doc comment describes
the bug it exists to fix: a compiled `getstatic` that is the first-ever access
to a class silently read `Value::Int(0)` instead of running `<clinit>`
(the JavaPoet `LineWrapper$FlushType` NPE). Sharing the bitmap across VMs
*recreates that exact bug* — VM A marking id 5000 initialized makes VM B skip
the check for its own, genuinely uninitialized, class 5000.

**Fix: an owner latch, not a per-VM table.** Each module gained
`static OWNER: AtomicUsize` (`0` = unclaimed) and an `owned_by(vm_identity)`
that CASes the claim on first use. The first VM to touch the table keeps the
one-relaxed-load fast path; every other VM answers `false` / falls through to
the authoritative `ensure_class_initialized_shared` and `class_manager` name
compare, forever.

Rationale for choosing the latch over a keyed table: these are the hottest
reads in `jit_getstatic` (the module doc measures `getstatic` at ~35 ns against
HotSpot's ~1, with these memos as the fix), so the steady-state cost of the fix
had to be one extra relaxed load and a compare. A per-VM table would need a
map lookup or a slot scan on every static read. **Fail-closed beats fast:** VM
#2 gets the correct slow path, and no shared mutable state can return a wrong
answer to anyone. `0` is a safe "unclaimed" sentinel because `NEXT_VM_IDENTITY`
starts at `1`.

### 3. `JIT_SCAN_CACHE` is keyed on the heap it was filtered through — `conservative_roots.rs`

This is the only cache in the directory that stores raw heap addresses
(`roots: Vec<ObjectRef>`), and it had no VM or heap component in its key at
all: `(filled_gen, chain_len, collection_count)`.

The instinct that `collection_count` covers it is wrong, and worth stating so
the next reader does not re-derive it: `collection_count` is
`heap.collection_count()` — a *different counter on each heap*. Two young heaps
agree on it trivially. So a thread that fills the cache while attached to VM A
and is then scanned by VM B's collector, at the same boundary generation and
chain depth, hands **VM A's object addresses to VM B's mark phase**. That is
the `oscache` bug from round 1, pointed at the collector instead of at a
descriptor table.

Fix: a `heap_id: usize` field (`conservative_roots.rs:1033`) set from
`jit_scan_cache_heap_id(heap) = heap as *const VmHeap as usize`
(`conservative_roots.rs:1086`), following the round-1 `heap_id` rationale verbatim —
`scan_active_jit_frames(scanner_sp, heap, out)` has a `&VmHeap` and nothing
else in scope, and everything stored is a raw address in that heap. The hit
test moved into `JitScanCache::matches` (`:1065`) so the keying is unit-testable
without a live VM, heap or JIT frame.

The residual is milder than round 1's: a recycled `VmHeap` address can only
produce a hit if `filled_gen`, `chain_len` **and** `collection_count` also
match, and a fresh heap's `collection_count` starts at `0`.

### 4. Address-derived VM keys upgraded to `vm_identity` — `helpers.rs`

Ten sites already carried a VM component, but built it as
`vm as *const SharedVm as usize`. All are now `vm.vm_identity`. This is not
theoretical: `SharedVm` allocations in a test process that constructs VMs
sequentially are a textbook allocator-reuse case, and unlike round-1's
`smuggled_longs` heap table, nothing sweeps these caches — a stale
`(recycled address, colliding class id)` pair is believed forever. The affected
caches are the three typecheck memos, the four `*_CLASS_CACHE` cells and
`INTEGER_ALLOC_SLOTS` (see the census table).

### 5. Deleted: `system_class_id` — `helpers.rs`

A `static CACHED: AtomicU32` holding `java/lang/System`'s `ClassId`, "resolved
once" for the whole process. It had **zero callers anywhere in the repository**
(`system_class_memo::is_system` superseded it), so this is a pure deletion, not
a behaviour change. A comment at the deletion site records why it must not come
back without a `vm_identity` in the key.

## Tests

All fail against the pre-fix code and pass after.

`vm/src/jit/helpers.rs` (`mod tests`):

* `jit_site_key_separates_vm_identities`
* `static_invoke_infos_share_one_address_across_vms` — builds two real
  `SharedVm`s and pins the premise: the six process-global `JitInvokeInfo`
  statics have one address across both.
* `dispatch_cache_does_not_serve_another_vms_compiled_entry`
* `virtual_dispatch_cache_does_not_serve_another_vms_compiled_entry` — same
  site *and* same receiver `ClassId` in both VMs, which is the collision that
  makes this miscompile-grade.
* `virtual_target_cache_does_not_serve_another_vms_class_name`
* `integer_native_dispatch_cache_is_vm_scoped`
* `dispatch_counter_is_vm_scoped`
* `class_init_memo_is_not_shared_between_vms`,
  `class_init_memo_owner_latch_is_claimed_once`
* `system_class_memo_is_owned_by_one_vm` — two real `SharedVm`s.

`vm/src/jit/conservative_roots.rs` (`mod tests`):
`scan_cache_hits_for_the_heap_it_was_filled_from`,
`scan_cache_does_not_hit_for_another_heap`,
`scan_cache_still_rejects_a_stale_generation_or_collection`,
`empty_scan_cache_matches_nothing_plausible`.

**No two-VM JIT-execution harness exists in this crate**, and building one was
out of scope: driving `jit_invoke_dispatch` needs compiled machine code, a live
`JvmThread` TLS binding and a pinned `CompiledMethod`. The tests therefore
exercise the *keying* directly — inserting under VM A's key and asserting VM B
misses — which is precisely the property each fix establishes. The two tests
that need real VMs use `SharedVm::new(VmConfig::default())`, the same
constructor `vm/src/vm/vm_init.rs`'s own `vm_identity_is_unique_per_shared_vm`
test uses.

The two owner-latched memos are process-global by construction, so the tests
that reset them take a module-local `MEMO_TEST_LOCK`. Nothing else in the unit
suite reaches those bitmaps — only `jit_getstatic` does.

## Open

### `JIT_SIGNALS.exception` — a thread-local `ObjectRef` with NO root provider (DANGEROUS)

`vm/src/jit/helpers.rs:303`, field `exception: Cell<Option<ObjectRef>>`.

This is the JIT→interpreter pending-exception handoff: a helper builds a Java
throwable, stores it here, and returns the `i64::MIN` deopt sentinel; the
interpreter drains it with `take_jit_pending_exception` and routes it through
the method's exception table.

**Neither half exists.** There is no `scan` and no `remap` for it anywhere:
`register_native_root_source` is not called from this directory, `VM_ROOT_SOURCES`
in `vm/src/memory/native_roots.rs` has no JIT row, and `vm/src/memory/gc.rs`
walks `thread.native_pending_return` (`:628`) and `thread.pending_async_exception`
(`:905`) — both **`JvmThread` fields**, which is exactly why they are reachable
from the collector. A `thread_local!` `Cell` is not.

The stashed throwable is often *also* a live Rust local, and while any JIT
frame is live `gc_quiescence` forces the non-moving young sweep — which removes
the *relocation* half of the hazard but not the *reclamation* half: an unmarked
object is still swept. The exposed windows are the ones where the TLS cell is
the only copy and Rust code runs on: `handle_compiled_callee_deopt_sentinel`,
`route_implicit_exc_through_callee` (`helpers.rs:2345-2400`, which re-executes
the callee in the interpreter), and the OSR `stash_jit_pending_exception`
re-post path (`:761`).

**Recipe.** Do NOT bolt a `root_source!` onto the thread-local: a root source
runs on the collecting thread and can only see *its own* TLS, so every parked
peer's pending exception would still be invisible — the same
"cross-thread JIT-root gap" this directory already has a detector for
(`conservative_roots.rs` `warn_cross_thread_jit_gap`). Two options, in
preference order:

1. **Move it onto `JvmThread`**, next to `pending_async_exception`. `gc.rs:905`
   already scans and remaps that field for every thread; a sibling field is one
   line each in `gc.rs` and costs the drain nothing (the drain already has the
   `&mut JvmThread`). This is the correct fix and touches `vm/src/threading/`
   or wherever `JvmThread` is declared, plus `vm/src/memory/gc.rs` — **both
   outside `vm/src/jit/`**, which is why it was not done here.
2. If the TLS cell must stay for ABI reasons, mirror
   `threading/thread_state.rs`'s `CELLS`: register each thread's signal block
   in a process-wide list of `Arc`s at thread attach, and scan/remap the list
   from a real `root_source!`. Round-1's warning applies — **both halves or
   neither**, and the remap must run for the right VM.

Either way the fix needs a test of the shape "set a pending exception, force a
young collection, drain and dereference".

### `jit_safepoint_slow_path` resolves the VM through `process_vm()` (isolation)

`vm/src/jit/helpers.rs:13243` calls `crate::native::jni::process_vm()` — the
`PROCESS_VM: Mutex<Option<Weak<SharedVm>>>` cell that round-1 named as
explicitly out of scope, with its "there is exactly one VM per process"
assertion. In a second VM, a cooperative safepoint poll from compiled code
therefore parks against the **first** VM's `safepoint_check`. Nothing in
`vm/src/jit/` can fix this; it is a symptom of the `PROCESS_VM` item, and it is
recorded here so that item's blast radius is known to include the JIT's
safepoint protocol.

### `xt_root_scan` takeover is a single process-wide arming table (concurrency, not keying)

`vm/src/jit/xt_root_scan.rs:821-826`. `INSTALL`/`ACTIVE`/`HELPER_MODE`/`SLOTS`/
`RANGES` are one set for the process. The *keying* is sound — slots are keyed
on the OS tid, which is process-unique, and `RANGES` mirrors the process-wide
JIT code arena — so nothing here returns a wrong answer for a wrong key. The
open question is whether two VMs' stop-the-world cycles can arm the table
concurrently: `ACTIVE` is a single `AtomicBool` and `SLOTS` is a shared pool of
`MAX_SLOTS`. Not investigated; it needs a two-VM STW soak, not a read. Listed
so the next sweep starts from the right framing.

### Residual on the owner latches

A process whose *first* VM is torn down leaves `class_init_memo` and
`system_class_memo` claimed by a dead identity, so every later VM takes the
slow path permanently. That is correct but slow. If it ever matters, add a
`forget_vm(vm_identity)` that clears the bitmaps and releases `OWNER` under the
same stop-the-world teardown transaction that calls
`release_vm_native_state` (`vm/src/vm/vm_init.rs:7282`) — the analogue of
round-1's unused `smuggled_longs::forget_heap`.
