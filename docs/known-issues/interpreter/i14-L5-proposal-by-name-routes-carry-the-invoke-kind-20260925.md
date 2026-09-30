# Proposal: the by-name invocation routes carry the invoke kind

**Status: open — filed 2026-09-25 by interpreter round i1 wave 14, lane L5.**

## Why

`vm/src/vm/vm_exec.rs::invoke_or_native` is reached by static, special,
virtual and interface calls alike and cannot tell them apart. Every
kind-specific rule therefore lives either at the caller (which must remember
to pick the right entry) or as a guess inside the route:

* JVMS §5.5 initialization: fixed three times in three waves for three
  static entries (wave 12 `jit_static_owner_init_before_native`, wave 13
  `invoke_static_or_native`, wave 14 the static tails
  `invoke_static_on_owner` / `invoke_static_shared` and
  `invoke_or_native_impl(.., static_call)`), and still open for
  `NativeContext::invoke`
  (`interpreter-L5-native-context-invoke-initializes-the-named-class-FIXED-20260925.md`);
  the bootstrap-method calls were a fourth, fixed in wave 15
  (`interpreter-L5-bootstrap-method-calls-initialize-the-symbolic-owner-FIXED-20260925.md`).
* The receiver-keyed arms at the top of `invoke_or_native_impl`
  (`DowncallHandle.type` / `invoke*`, `AnnotationProxy`, `Proxy$Instance`,
  the `ClassLoader` intercepts) test `args[0]` for "is this a receiver", and
  the two `DowncallHandle` arms take a class-manager read to learn the
  answer on every `invoke`/`invokeExact`/`invokeBasic`/`type()` call with an
  object first argument. A static call has no receiver; the kind would answer
  that without a lock.
* `invoke_on_class_shared_inner`'s retarget ("never retarget a STATIC
  method") re-derives staticness by a hierarchy walk
  (`find_method_recursive(..).map(|(m, _)| m.is_static())`) that the caller
  already knew.

## Design

A `ByNameKind { Static, Special, Virtual, Interface }` parameter on one
internal entry (`invoke_or_native_impl`, which already takes `static_call`),
with the public wrappers fixing it: `invoke_static_or_native` (Static),
`invoke_special_shared` (Special), `invoke_or_native` (Virtual — its
callers are the JIT virtual tails and reflection's `invoke_virtual`). The
kind then:

1. selects the tail (static: declaring-class initialization; special:
   no retarget; virtual: receiver retarget);
2. skips the receiver-keyed arms for `Static`;
3. is passed down to `invoke_on_class_shared_inner` so the static/no-retarget
   questions stop being recomputed.

## Staged plan

1. Replace `static_call: bool` with the enum; `invoke_or_native` keeps
   `Virtual` semantics exactly (byte-for-byte for `--compatible`).
2. Skip the receiver-keyed arms for `Static` (a pure fast path: a static
   call's `args[0]` is never a receiver, so those arms never legitimately
   matched it).
3. Move `NativeContext`'s static callers onto a kind-carrying trait method
   after the census in the `i14-L5-native-context-invoke-*` page.
4. Thread the kind into `invoke_on_class_shared_inner` and delete the
   static re-derivations there.

## Expected benefit

Correctness by construction for the class of bug that took three waves; one
class-manager read less per by-name `MethodHandle` call with an object first
argument; a smaller surface for the next kind-specific rule.

## Verify

The wave-14 `i14_l5_static_tail_tests`, the probes
`tools/probes/interp/L3/JitInheritedStaticInit.java` and
`InheritedStaticRoutes.java`, and the core / jdk-only suites JIT on and off
after each stage.

## Progress (wave 15)

Stage 3 landed in a different shape, without a trait method: the door
classifies the resolved method itself. `NativeContextImpl::invoke` →
`native_context_invoke` (`vm/src/vm/vm_exec.rs`) asks
`invokestatic_init_class` on a not-yet-initialized named class and takes the
static tail `invoke_static_on_owner` for an inherited static, under the
per-VM predicate `native_invoke_static_tail_enforced` (`--jdk-only` today;
the `--compatible` flip waits on the census in
`interpreter-L5-native-context-invoke-initializes-the-named-class-FIXED-20260925.md`).
The loader-faithful `REF_invokeStatic` lambda arms got their own static entry
(`invoke_static_on_class_no_retarget`). Stages 1, 2 and 4 are untouched; the
enum would let `native_context_invoke`'s classification go away for callers
that know the kind.

## Progress (wave 16)

Interpreter round i1 wave 16, lane L3:

- **Stage 1 landed.** `vm/src/vm/vm_exec.rs`: `enum ByNameKind { Static,
  Virtual }` replaces `invoke_or_native_impl`'s `static_call: bool`;
  `invoke_or_native` passes `Virtual` (every arm asked, exactly as before),
  `invoke_static_or_native` / `invoke_static_or_native_prepared` pass
  `Static`. Only the two kinds with a caller exist: `Special` has its own
  body (`invoke_special_shared_impl`, which still takes a `static_call`
  bool of its own) and `Interface` has no entry that could tell it from
  `Virtual`; add them with their first caller.
- **Stage 2 landed.** `ByNameKind::may_have_receiver` gates the eight
  receiver-keyed arms of `invoke_or_native_impl` (`DowncallHandle.type`,
  `DowncallHandle.invoke*`, `AnnotationProxy`, `Proxy$Instance`,
  `setDefaultAssertionStatus`, the null-name `getResource*`,
  `ClassLoader.loadClass`, the `To*Function.apply` redirect): a `Static`
  call skips them. Seven of the eight could not match a genuine static call
  anyway (no object `args[0]`, or a dispatch class with no such static); the
  `invoke*` arm took the class manager on every static call named `invoke` /
  `invokeExact` / `invokeBasic` with an object first argument, and would
  have run the downcall native for a static user method whose first argument
  was a `DowncallHandle`. `--compatible` is otherwise unchanged. The Spring
  Boot loader arms (`ZipContent$SignatureFiles.<clinit>` is static) are not
  receiver-keyed and stay asked for both kinds.
- **Stage 3, for method handles.** `NativeInvokeAccess` gained
  `invoke_static` / `invoke_static_by_class_id` (`native-api`
  `registry.rs`; defaults fall back to `invoke` / `invoke_by_class_id`), and
  the static arm of `mh_dispatch` (`native-builtins/src/lang_invoke.rs`)
  calls them: a `findStatic` handle initializes the declaring class in every
  mode (`docs/internal/fixed-bugs/interpreter-L0-lookup-find-static-initializes-the-named-class-FIXED-20260925.md`).
  `NativeContext::invoke`'s other callers keep `native_context_invoke`'s
  classification until its census.
- Tests: `vm_exec.rs` module `i16_l3_native_lambda_tests`
  (`only_a_virtual_by_name_call_asks_the_receiver_keyed_arms` pins the
  eight gates), the `i14_l5_static_tail_tests` static-tail tests (unchanged
  behaviour through the enum) and the two new `invoke_static*` tests there.
- Bench: `tools/probes/interp/L3/ByNameCallBench.java` (rows `mh-static`,
  `mh-inherited`, `reflect-*`, `mh-virtual`, `lambda`). Stage 2 removes a
  lock only from static calls named `invoke*` with an object argument, which
  that bench does not isolate; expect it flat.

Next: stage 4 (thread the kind into `invoke_on_class_shared_inner` so its
"never retarget a static" walk stops re-deriving staticness), and moving
`NativeContext`'s remaining static callers (reflection's static
`Method.invoke`, `native-builtins`' `ctx.invoke` of statics) onto
`invoke_static` once `native_invoke_static_tail_enforced`'s census reads
zero.

## Progress (wave 20)

Interpreter round i1 wave 20, lane L3 — the inherited-static cost wave 16
named (`mh-inherited`: the declaring-class initialization check re-resolved
the method on every call, because the symbolic owner of an inherited static
is never initialized itself, so `static_owner_init_pending`'s initialized
memo never answers for it):

- **A `findStatic` handle remembers its settled owner.**
  `native-builtins/src/lang_invoke.rs`: a seventh synthetic slot,
  `MH_STATIC_SETTLED` (`MH_BASE + 6`, minted by `alloc_method_handle`,
  width-guarded like `MH_VARARGS`, `Int(0)` until settled). The static arm
  of `mh_dispatch` reads it before any GC point, pins the handle only while
  it is unsettled, and calls the new
  `NativeInvokeAccess::invoke_static_settling(owner, settled, ..)`
  (`native-api/src/registry.rs`; the default runs `invoke_static` /
  `invoke_static_by_class_id` and never settles, so every mock and other
  context behaves as before). A handle too narrow for the slot takes the two
  routes directly.
- **The VM side** (`vm/src/vm/vm_exec.rs::invoke_static_settling`, and the
  `NativeContextImpl` override): the owner is found exactly as the two
  static tails find it (the recorded `--jdk-only` owner id, or
  `load_class_pinning_args` by name). When it equals the handle's remembered
  owner the call is the tail's "nothing to initialize" arm
  (`invoke_on_class_shared`) without asking `static_owner_init_pending`;
  otherwise it is `invoke_static_on_owner` exactly, and the owner is
  reported settled once a SUCCESSFUL call leaves nothing to initialize for
  it (`static_owner_init_pending(..).is_none()`). Initialization is monotone
  and a live class's hierarchy is fixed (redefinition cannot change it;
  class ids are not reused after unloading), so the answer holds for every
  later call on that owner. JVMS §5.5 order is unchanged: the first call —
  and every call while the declaring class is being initialized by this
  thread's own `<clinit>`, or after its initialization failed — takes the
  full tail. A remembered owner is only ever trusted for the owner the call
  resolves, so a copied handle (`mh_clone_handle`) or a by-name owner that
  later resolves elsewhere costs the fast path, never an initialization.
- Tests: `vm_exec.rs` module `i14_l5_static_tail_tests`,
  `a_settling_static_dispatch_initializes_the_declaring_class_once_then_settles`
  (P.<clinit> once, C.<clinit> never for the inherited static, settled
  owner reported and honoured by name and by id, a foreign remembered owner
  ignored); `lang_invoke.rs`
  `a_minted_handle_starts_unsettled_and_remembers_its_settled_owner`.
- Bench: `tools/probes/interp/L3/ByNameCallBench.java` (header updated):
  `mh-inherited` should get faster, `mh-static` flat or slightly faster
  (one initialized-memo probe less per call), the other rows are controls.

Stages 1, 2 and 3 (for method handles) stand as recorded above. Next:
stage 4 (thread the kind into `invoke_on_class_shared_inner`, whose "never
retarget a static" hierarchy walk still runs on the settled path whenever
the first argument is an object of another class); and the per-call string
reads of `mh_dispatch` (`mh_read_class` / `_name` / `_desc` allocate three
`String`s per call), which now dominate a settled static handle's by-name
cost — `i13-L5-proposal-general-resolver-by-name-memo-20260925.md` is the
place for that.
