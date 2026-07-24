# `invoke_virtual` lambda-dispatch: `receiver`/`args` held unpinned across `lambda_args_sam_compatible`'s GC-triggering class-loading calls — FIXED (partial contributor to WFLYCTL0079 during `parallel-extension-add`)

Status: FIXED — 2026-07-15

Date observed/fixed: 2026-07-15, isolated worktree `/data/data/wt-io0079-20260715`, forked from
`origin/dev @ 3a6bd4a6`.

## Symptom

Investigating `WFLYCTL0079: Failed initializing module org.wildfly.extension.io` during WildFly
standalone boot's `parallel-extension-add` step (see
`docs/known-issues/wildfly-standalone-boot-attributeaccess-cce-register-invisible-root.md` for the
full family writeup and this doc's relationship to it). Live `CRATONVM_DBG_STALE_OBJREF=1
RUST_BACKTRACE=1` repro captures consistently showed a hard panic with this backtrace shape:

```text
thread 'Thread' panicked at gc/src/gen_heap.rs:1564:13:
CRATONVM_DBG_STALE_OBJREF: stale ObjectRef detected at <OLD> — this object was evacuated by a moving GC
to <NEW> (class_id=<N> kind=Object), but native/interpreter code dereferenced the OLD address...
   7: get_header
   8: class_id_of
   9: class_id_of
  10: lambda_arg_provably_not_instance      (vm/src/runtime/interpreter.rs:19034)
  11: checkcast_lambda_instantiated_args    (vm/src/runtime/interpreter.rs:18986)
  12: coerce_lambda_args                    (vm/src/runtime/interpreter.rs:19281)
  13: invoke_virtual                        (vm/src/vm/vm_exec.rs:6850, pre-fix line)
  14: invoke_deferred_stream_lambda         (native-collections/src/lib.rs:12092)
  15: stream_process_chain                  (native-collections/src/lib.rs:12190)
  ...
  21: native_stream_to_array_gen            (native-collections/src/lib.rs:14322)
```

i.e. a `java.util.stream` pipeline's `.toArray()` call, processing a deferred `map`/`filter` lambda,
ends up replaying a `checkcast` against a stale receiver and throwing
`ClassCastException: java.lang.Object cannot be cast to X` for whatever `X` the lambda's instantiated
SAM parameter type happened to be. Surfaced widely during `parallel-extension-add` because several
`wildfly-controller` classes (`SimpleResourceDefinition.getAddOperationParameters`,
`CapabilityRegistry.registerPossibleCapability`, `CapabilityRegistration.<init>`,
`ConcreteResourceRegistration.registerOperationHandler`, various `PersistentResourceXMLDescription`
builders) are written in a stream-heavy style over collections of `AttributeDefinition`/
`AttributeAccess`/`RegistrationPoint`/etc.

## Root cause

`vm/src/vm/vm_exec.rs::invoke_virtual` — the lambda-proxy dispatch path (taken when the receiver's
class is a registered `lambda_proxies` entry) decides whether to intercept the call as the lambda's
SAM via a `.filter()` predicate:

```rust
if let Some(lcs) = call_site.filter(|lcs| {
    method_name == &*lcs.sam_method_name
        && ...arity check...
        && crate::runtime::interpreter::lambda_args_sam_compatible(self.shared, &lcs.sam_descriptor, args)
}) {
    let num_captures = lcs.capture_types.len();
    let mut full_args: Vec<Value> = Vec::with_capacity(num_captures + args.len());
    for i in 0..num_captures {
        full_args.push(self.shared.heap.get_field(receiver, i));   // reads `receiver`
    }
    full_args.extend_from_slice(args);                              // reads `args`
    ...
    coerce_lambda_args(self.shared, self.thread, ..., &mut full_args, ...)?;
```

`lambda_args_sam_compatible` (`vm/src/runtime/interpreter.rs:19375`) checks each reference-typed SAM
parameter against the actual argument's runtime class via `lambda_proxy_satisfies`/
`synthetic_implements`/`proxy_instance_satisfies_target`/`annotation_proxy_satisfies_target` — the same
helper family already flagged (in a sibling function's own GC-safety comment) as capable of triggering
class loading, i.e. a GC-triggering call.

`receiver` (an `ObjectRef`) and every object element of `args` (a `&[Value]`) are plain Rust locals at
this point in `invoke_virtual` — **not** pinned into `self.thread.native_pin_roots` before the
`.filter()` predicate runs. If a moving GC lands during `lambda_args_sam_compatible`'s class-loading
work, both go stale. The subsequent `get_field(receiver, i)` / `extend_from_slice(args)` reads (used to
build `full_args`) then copy the *stale* values into `full_args`, which is what's ultimately handed to
`coerce_lambda_args` → `checkcast_lambda_instantiated_args` → `lambda_arg_provably_not_instance`. Those
three downstream functions already do their own careful pin/refresh dance (per their own GC-safety
comments), but pinning an already-stale value doesn't recover the correct one — garbage in, garbage
pinned. The eventual `class_id_of` read on the (still-stale) value is what actually panics/corrupts.

This is a `parallel-extension-add`-relevant instance of the same general "stale native `ObjectRef`
across a GC-triggering call" family documented in
`docs/known-issues/wildfly-parallel-boot-stale-objectref-residual.md`, at a call site (`invoke_virtual`'s
own lambda-dispatch decision point) not covered by any of that investigation's previously-fixed sites.

## Fix

Pin `receiver` and every object element of `args` into `self.thread.native_pin_roots` **before** calling
`call_site.filter(...)` (i.e. before `lambda_args_sam_compatible` can trigger a GC), then re-read both
through the pins immediately after the predicate returns — in both the `Some(lcs)` (lambda-dispatch)
branch, where a fully-refreshed `full_args` is built from the pinned values instead of the original
locals, and the `else` (non-lambda fallback dispatch) branch, where `receiver` alone is refreshed
(cheap, unconditional insurance for the rarer "receiver is a lambda proxy but the SAM-compatibility
check said no" path). See `../../../../vm/src/vm/vm_exec.rs`, `invoke_virtual`, the `sam_compat_pin_base`/`arg_pins`
block.

## Verification

- `cargo check -p cratonvm-vm`: clean.
- `cargo test -p cratonvm-vm --lib --release`: **2202 passed / 9 failed**, byte-for-byte identical
  before (`git stash`) and after this fix on the same tree (`origin/dev @ 3a6bd4a6`). The 9 failures are
  all `runtime::lock_order::tests::*` — pre-existing, expected failures when running `--release` (they
  assert `debug_assertions` is active), unrelated to this change and unaffected by it.
- Live repro: the specific stale-`ObjectRef` panic captured pre-fix (backtrace above, `invoke_virtual` at
  the pre-fix line pointing at the unprotected `args`/`receiver` reads) does not recur at that call
  shape post-fix — post-fix panics captured with the same `CRATONVM_DBG_STALE_OBJREF=1` diagnostic show
  `invoke_virtual`'s frame now pointing at the post-fix `coerce_lambda_args` call site (i.e. execution
  correctly passes through the newly-added pin/refresh block), confirming the fix is live and exercised.
- **Does not fully close the residual.** Matched 12-attempt plain repro batches (no diagnostic flag,
  default JIT-on, real JDK25) before and after this fix both show `WFLYCTL0079`-via-`ClassCastException`
  at roughly the same rate (5/12 baseline, 5/12 fixed) — this fix closes one genuine, confirmed
  contributing site but the family has other, deeper contributors. See
  `docs/known-issues/wildfly-standalone-boot-attributeaccess-cce-register-invisible-root.md`'s
  2026-07-15 follow-up section for the full characterization of what remains open, including new
  evidence that the residual is **not** simply another missed-pin site.

## Related

- `docs/known-issues/wildfly-standalone-boot-attributeaccess-cce-register-invisible-root.md` — the
  primary open tracking doc for this symptom family; this fix is one confirmed, narrow contributor,
  not a full resolution. Read that doc's 2026-07-15 follow-up section for what's still open.
- `docs/known-issues/wildfly-parallel-boot-stale-objectref-residual.md` — the general "Family 1"
  unpinned-native-local pattern this fix is an instance of.
- `docs/internal/fixed-suite-bugs/wildfly-remoting-classcastexception-parallel-extension-add.md` — a
  sibling fix (JIT `checkcast`/`instanceof` slow path) for the byte-for-byte identical symptom shape,
  different call site.
