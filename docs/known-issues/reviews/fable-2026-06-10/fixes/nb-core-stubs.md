# Fix note: nb-core-stubs

Agent: nb-core-stubs
Owned file: `native-builtins/src/phases_late.rs` (only)
Report: `docs/reviews/fable-2026-06-10/nb-core.md`

## Scope reconciliation

The task brief listed B2–B8 as if they all lived in `phases_late.rs`, but the
review pins several to files I do **not** own. I fixed only the ones in my owned
file and left the others for their owners:

| Bug | Location per report | In my owned file? | Action |
|-----|---------------------|-------------------|--------|
| B2 (locale "all" → ROOT) | `lib.rs:284` | No | Out of scope — see "Deferred" |
| B3 (Scanner(InputStream) discards input) | `phases_early.rs:1992` | No | Deferred |
| B4 (Scanner UTF-8 byte slicing) | `phases_early.rs:2039…` | No | Deferred |
| **B5** (Future/CF executor ignored) | `phases_late.rs` | **Yes** | Fixed (documented limitation) |
| **B6** (SubmissionPublisher no delivery) | `phases_late.rs` | **Yes** | Fixed (real delivery) |
| **B7** (Charset.forName accepts anything) | `phases_late.rs:226` | **Yes** | Fixed (fail-closed) |
| B8 (MessageFormat typed elements) | `phases_early.rs:7555` | No | Deferred |

## What I changed (all in `native-builtins/src/phases_late.rs`)

### B6 — SubmissionPublisher now delivers to subscribers (primary fix)

Previously `submit`/`offer` stored only the last item and returned a hardcoded
`1`; `subscribe`/`hasSubscribers`/`getNumberOfSubscribers` were no-ops/zeros.
Reactive-Streams consumers silently received nothing.

New behavior (`register_p60_flow`, helpers above it):
- `subscribe(Flow.Subscriber)` — stores the subscriber, allocates a
  `Flow$Subscription`, and calls `subscriber.onSubscribe(subscription)`.
  Null subscriber → NPE (was a silent no-op).
- `submit(Object)` — delivers the item to every registered subscriber via
  `onNext`, returns the live subscriber count as the (synchronous) estimated
  lag. With no subscribers this is `0`, not a fake `1`. Throws
  `IllegalStateException` if the publisher is closed.
- `offer(Object, BiPredicate)` — same delivery; returns `-1` on a closed
  publisher (JDK convention).
- `hasSubscribers` / `getNumberOfSubscribers` — read the real subscriber list.
- `close()` — (see caveat) fires `onComplete` to each subscriber.

New helpers: `sp_wrapper_ensure`, `sp_subscribers`, `sp_append_subscriber`,
`sp_deliver_on_next`. All re-entrant `invoke_virtual` calls pin `this` / the
item / the subscriber across the call and re-read the forwarded ref (moving-GC
safe, mirrors the established `pin_native_root` pattern at `Class.getModule`).

Also removed the now-redundant `subscribe`/`hasSubscribers`/
`getNumberOfSubscribers` no-op/zero re-registrations in
`register_p69_submission_publisher` — they ran *after* phase 60 and would have
clobbered the working delivery path (registry `methods.insert` = last wins).
`getMaxBufferCapacity` / `getClosedException` there are genuinely new and kept.

#### CROSS-FILE COUPLING (important, owner action needed in lib.rs)

`native-builtins/src/lib.rs` `register_t31_concurrent_extras` (~line 21216)
ALSO registers `SubmissionPublisher` `<init>()V`, `close()V`, `isClosed()Z`,
and it is registered **last** in the boot sequence (lib.rs call order:
`register_concurrent_natives` → `register_phase60_natives` →
`register_phase69_natives` → … → `register_t31_concurrent_extras`), so the
lib.rs versions win over the phase-60 ones at runtime.

Consequences I designed around:
- The lib.rs `<init>()V` sets publisher **field 0 = an ArrayList-style WRAPPER
  object** (wrapper field 0 = ref array cap 8, wrapper field 1 = Int count),
  field 1 = closed. There is no `lastItem` field. I therefore rewrote all my
  helpers to read/write subscribers **through that wrapper** (and to lazily
  create an identical wrapper if field 0 is null, so the phase-60 fallback
  `<init>` is also compatible). `submit`/`offer`/`subscribe` are *not*
  registered by lib.rs, so my delivering versions are the ones that run — B6 is
  genuinely fixed for the submit→onNext path.
- The lib.rs `close()V` only flips the closed flag and does **not** fire
  `onComplete`. Because it wins, my `onComplete`-on-close body does not execute
  at runtime (it remains as a correct fallback). **Recommended lib.rs follow-up
  (owner of lib.rs):** either delete the lib.rs `SubmissionPublisher`
  `<init>/close/isClosed` block so the phase-60 versions (which fire
  onComplete) take effect, or add the `onComplete` fan-out to the lib.rs
  `close`. Keep the wrapper field-0 layout in sync if you touch `<init>`.

### B7 — Charset.forName fails closed for unknown charsets

`forName` already rejected names that `normalize_charset_name` (lib.rs) maps to
empty; I tightened the thrown message to the JDK `UnsupportedCharsetException`
message format (the bare charset name) and documented why we raise
`IllegalArgumentException`: the VM has no `UnsupportedCharsetException`
`RuntimeError` variant, and in the real JDK
`UnsupportedCharsetException extends IllegalArgumentException`, so callers that
catch `IllegalArgumentException` behave correctly. A precise type would require
a new variant in `types/src/error.rs` (not my file) — noted, not silently wrong.

NOTE: the over-loose `"contains UTF and 8 → UTF-8"` heuristic that lets a few
bogus names through lives in `normalize_charset_name` in **lib.rs** (not mine);
genuinely-unknown names already return empty and are now rejected here.

### B5 — Future/CompletableFuture executor ignored

Kept result-correctness (the supplier/runnable always runs, the CF completes
with the correct value) and replaced the silent "Executor argument ignored"
comments on the `supplyAsync(Supplier,Executor)` and `runAsync(Runnable,
Executor)` overloads with explicit, accurate documentation of the limitation:
the task runs eagerly on the calling thread, so executor thread/affinity/
parallelism is not honored. I deliberately did **not** route through
`executor.execute(...)` because an asynchronous executor would return before
producing the result, leaving the CF marked done with no value — that would
trade a scheduling difference for a correctness bug. A faithful fix needs the
real carrier scheduler (`SharedVm.virtual_scheduler`), which is out of scope for
this file. This satisfies the brief's "keep correctness and note it" bar.

## Deferred (not my owned files) — for whoever owns them

- B2 `lib.rs:284` `essential_quarkus_locale_convert("all")` → should read the
  `Locale.ROOT` static field, not `Locale.getDefault()`.
- B3 `phases_early.rs:1992` Scanner(InputStream) should drain the stream.
- B4 `phases_early.rs:2039…` Scanner string slicing should be char-boundary safe.
- B8 `phases_early.rs:7555` MessageFormat should honor typed sub-formats.
- B7 residual: tighten `normalize_charset_name` heuristic in `lib.rs:23778`.

## Tests added

New `#[cfg(test)] mod nb_core_stubs_fix_tests` at end of `phases_late.rs`:
- `b6_submission_publisher_core_methods_registered` — submit/subscribe/
  hasSubscribers/getNumberOfSubscribers are registered.
- `b6_subscribe_grows_subscriber_list_and_counts` — two subscribes grow the
  wrapper-backed list; count + backing array order verified.
- `b6_subscribe_null_subscriber_throws_npe` — null subscriber throws.
- `b6_submit_to_closed_publisher_throws` — submit on closed publisher throws
  (was a fake lag return).
- `b6_submit_with_no_subscribers_returns_zero_lag` — lag 0, not hardcoded 1.
- `b7_charset_for_name_rejects_unknown` — unknown charset throws.
- `b7_charset_for_name_accepts_known` — "utf-8" → Charset whose name is "UTF-8".

Tests use the existing `test_utils::mock_ctx` / registry `find` harness. The
mock's `invoke_virtual` is a stub (no real Java dispatch), so the tests verify
list/state management and error paths rather than observing onNext arriving in a
Java subscriber; the delivery loop itself is exercised structurally.

## Risk

- Behavioral change for `SubmissionPublisher.submit/offer/subscribe/
  hasSubscribers/getNumberOfSubscribers`: previously inert, now deliver. Any
  caller depending on the old no-op behavior would see different (correct)
  results. `close` runtime behavior unchanged (lib.rs wins).
- `Charset.forName` now throws for unknown names where it may previously have
  returned a synthetic Charset for borderline names — this is the intended
  fail-closed correction.
- B5 is behavior-neutral (comments + same code path).
- Did not run cargo/git per instructions; edits mirror existing types/APIs.
