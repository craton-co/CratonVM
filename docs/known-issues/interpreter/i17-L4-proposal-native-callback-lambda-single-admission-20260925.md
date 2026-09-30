# Proposal: admit and root a native-callback lambda call once, not twice

**Status: open — filed 2026-09-25 by interpreter round i1 wave 17, lane L4.**

## Where it stands

Wave 17 collapsed the native-callback lambda dispatch onto the interpreter's
`try_lambda_dispatch`
(`docs/internal/fixed-bugs/interpreter-L5-native-callback-lambda-dispatch-is-a-second-copy-FIXED-20260925.md`).
A native's `ctx.invoke_virtual(proxy, sam, ..)` — `Method.invoke` on a lambda,
every stream / collection native that calls a functional interface — now runs
the interpreter's cached paths (`lambda_global_impl_owner`,
`try_invoke_cached_lambda_impl`). What is left on that door is duplicated
bookkeeping around the one dispatcher:

`vm/src/vm/vm_exec.rs`, `NativeContextImpl::invoke_virtual`, lambda arm:

1. pins the receiver and every object argument (`sam_compat_pin_base`,
   `arg_pins: Vec<Option<usize>>`, one `Vec` per call);
2. asks `lambda_accepts_descriptor` (the filter on `call_site`);
3. copies the arguments into `refreshed_args: Vec<Value>` (a second `Vec`);
4. `native_callback_lambda_via_interpreter` pins the receiver and the
   arguments AGAIN (`push_object_arg_pins`), for the serialization-hook read
   after the dispatch;
5. `try_lambda_dispatch` asks `lambda_accepts_descriptor` again (its SAM-name
   guard) and builds `full_args` (a third `Vec`).

Nothing between 1 and 4 can collect (the predicate reads side tables only;
the existing comment says so), so the first pin set and the copy exist only
to hand the arguments across a non-GC-point.

## Design

- Pin once: drop step 1's pins and `refreshed_args`; call
  `native_callback_lambda_via_interpreter` with `receiver` / `args` directly
  after the filter (it pins for the dispatch). Keep the recovery
  (`recover_stale_lambda_receiver_from_native_pins`) that runs before the
  filter unchanged.
- Admit once: an `admitted: bool` (or a `try_lambda_dispatch_admitted`
  entry) that skips the dispatcher's own `lambda_accepts_descriptor` call
  when the caller has just asked it with the same `(proxy, name, descriptor)`.
- Optionally a `SmallVec<[Value; 8]>` for `full_args` in `try_lambda_dispatch`
  (every door benefits).

## Expected benefit

Two `Vec` allocations, one pin/truncate cycle and one descriptor predicate
per native-callback lambda call. Worth doing only if the measurement below
shows the door is hot; `lambda-prof`'s `prep` bucket covers step 5 only.

## Staged plan

1. Measure: a `*Bench*` probe that drives a lambda through a native (e.g.
   `Method.invoke` on a `Supplier` lambda's `get`, or a native sort with a
   `Comparator` lambda) with `CRATONVM_DBG=lambda-prof`, before and after.
2. Pin once (a GC-safety argument, reviewed against the collector's native
   pin contract).
3. Admit once.

## Verify

`i17_l4_native_lambda_tests` and `i16_l3_native_lambda_tests` in
`vm_exec.rs`; the core and jdk-only suites JIT on and off; the pin watermark
assertion in those tests (`pins released`).

## Risk

Low. The one subtle part is step 2: prove no allocation or safepoint sits
between the filter and the dispatcher's own pins.
