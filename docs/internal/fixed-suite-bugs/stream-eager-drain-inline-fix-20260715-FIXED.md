# FIXED and retired on 2026-07-15

All runtime residuals named in this record are now closed on `dev`.

## Final closure verification

The original fix (`cf45bb80`, merged through `07d2080b`) is retained unchanged:
reference streams sourced by a live spliterator drive one source element through
their complete deferred operation chain before the next `tryAdvance` call. This
final closure used a fresh release build of current `origin/dev` in the isolated
worktree `fix/keycloak-stream-eager-drain-residuals-20260715-001` and the unique
binary `cratonvm-keycloak-stream-eager-drain-residuals-20260715-001`.

- `cargo test -p cratonvm-native-collections --lib`: 72 passed, 0 failed.
- A focused cursor/infinite-source probe passed identically on HotSpot 17 and
  CratonVM: a live `StreamSupport.stream` cursor stopped after exactly seven
  elements through `limit()`, and `Stream.iterate`/`Stream.generate` limits
  produced their exact expected lists. It also exercised deliberately
  state-advancing `Spliterator.OfInt`, `OfLong`, and `OfDouble` sources through
  `StreamSupport.intStream`/`longStream`/`doubleStream`; each emitted exactly
  `[0,1,2,3,4]` with no over-read. Thus the primitive-stream item was a
  speculative coverage gap, not an unfixed runtime defect.
- Selenium 4.39.0's actual `Json`/`MapCoercer` path passed on both VMs for flat,
  nested, escaped, and a 512-key nested-map payload (516 decoded fields total).
  This directly exercises the `JsonInputIterator` shape that motivated the
  original report and no longer produces `JsonException`.
- The preserved `WelcomePageTest` runner completed all six methods and reached
  `RUN-END` in 258 ms under the fresh VM, with no stream JSON exception, hang,
  or orphaned process. Its remaining failures are uninitialized framework
  injection fields and unavailable floating-IP configuration: the preserved
  runner no longer provisions Keycloak's external test server on this host.
  They are fixture/environment failures, not a CratonVM residual. The earlier
  provisioned run recorded the actual Keycloak 26.6.1 server boot and clean
  teardown after the zipfs repair.

No open CratonVM issue remains in this family. The prior wording below is kept
as the implementation and historical investigation record.

---

# Stream/Spliterator eager-drain fix: lazy sources now drive their op-chain inline (2026-07-15)

Status: FIXED, merged to `dev`. Follow-up to
`../../known-issues/keycloak/welcomepagetest-stream-spliterator-zipcopy-residuals-20260715.md`
(section 2, "Selenium/HtmlUnit JSON parsing failure — ROOT-CAUSED, NOT FIXED"), which root-caused
this bug but deliberately left it unfixed pending a dedicated session. This doc records the fix,
what was verified, and residual scope.

## What was wrong

CratonVM's synthetic `java.util.stream.Stream` pipeline (`native-collections/src/lib.rs`) already had
a "deferred op-chain" mechanism (the keycloak-16 Part B work: `stream_try_defer` /
`stream_make_lazy_derived` / `stream_process_chain`) that correctly threads one element through the
whole downstream chain (map -> filter -> peek -> ...) in a single interleaved step, matching HotSpot's
pull-based semantics — but only once it had an element in hand.

For a stream sourced from `StreamSupport.stream(spliterator, false)` (e.g. every
`Spliterators.spliteratorUnknownSize(iterator, 0)` call), TWO call sites still obtained that "element
in hand" by fully draining the RAW source spliterator first, via `drain_spliterator_to_array`'s dumb
append-only `tryAdvance` loop, before any chain op ever ran on ANY element:

1. `stream_make_lazy_derived` (called by every `.map()`/`.filter()`/`.peek()`/`.limit()`/`.skip()`/
   `.flatMap()`) unconditionally called `materialize_lazy_stream` on its `src` BEFORE recording the
   new op onto the chain — so the very first intermediate op on a lazy stream triggered the raw drain,
   with an empty (or not-yet-extended) chain in effect.
2. `stream_pull_internal` / `stream_pull_synthetic_downstream` (the terminal-op pull loop, used by
   `collect`/`toArray`/`count`/`forEach`/`sorted`/`findFirst`/`anyMatch`/etc., and by `flatMap`'s
   inner-stream drive) obtained their base elements via `stream_source_elems` ->
   `materialize_lazy_stream`, i.e. the same raw drain, before looping `stream_process_chain` over the
   result.

Both are invisible for the common case (`Iterator.next()` returns an independent, fully-realized value
each call) but break the "cursor" idiom: `next()` returns `this` (the same object every call), and the
real per-element consumption/advancement happens inside a downstream op (map/filter/peek), not inside
`next()` itself. `Cursor.hasNext()` legitimately depends on state that only changes once that
downstream processing runs — but during the raw drain, nothing has been processed yet, so `hasNext()`
never observes a change and never returns `false`. The drain ran until
`drain_spliterator_to_array`'s hard-coded 1,000,000-iteration safety cap, at which point downstream
processing ran against the (now-exhausted) cursor and threw (typically `ArrayIndexOutOfBoundsException`
from the mapper reading past the end of its backing list).

This is the exact shape of `org.openqa.selenium.json.JsonInputIterator`
(`org.openqa.selenium.json.MapCoercer`), which drives Selenium's own JSON `Map` decoding.

## The fix

`native-collections/src/lib.rs`:

- **`stream_make_lazy_derived`**: no longer eagerly drains `src`. If `src` still holds a live,
  undrained lazy spliterator (`STREAM_FIELD_LAZY_SPLITERATOR`, slot 2), that spliterator is propagated
  forward, UNDRAINED, onto the new derived stream, alongside the extended op-chain. This single choke
  point covers `.map()`/`.filter()`/`.peek()`/`.limit()`/`.skip()`/`.flatMap()` uniformly, since all of
  them funnel through `stream_try_defer` -> `stream_make_lazy_derived`.
- **New**: `drain_spliterator_inline` — drives a lazy spliterator's `tryAdvance` loop directly, handing
  each element to a new `cratonvm/internal/StreamChainCollector` consumer whose `accept(Object)V`
  native (`native_stream_chain_collector_accept`) runs that ONE element straight through the stream's
  op-chain (`stream_process_chain`) before the loop asks for the next element. This is what makes the
  drive genuinely pull-based/interleaved instead of a disconnected raw drain, and it stops calling
  `tryAdvance` the moment the chain is satisfied (a `limit`, or the ultimate terminal short-circuiting),
  so short-circuiting terminals over a lazy source now genuinely short-circuit.
  - The one piece of machinery this needed that didn't already exist: a way for the terminal's
    `emit` closure (arbitrary per call site — accumulate into a `Vec`, check a predicate, stop after
    one element, etc.) to be reached from the reentrant `accept()` native, which is invoked from
    inside a `tryAdvance` call that re-enters Java. This uses a thread-local stack of "pull frames"
    (`SPLITERATOR_PULL_STACK` / `SpliteratorPullFrame` / `PullFrameGuard`) holding a raw pointer to the
    `&mut dyn FnMut(...)` closure, scope-guarded by an RAII guard that pops the frame unconditionally
    (including on error, via `Drop`) before `drain_spliterator_inline` returns by any path — the same
    established pattern this file already uses for `ChmMonitorGuard` (a `&mut dyn NativeContext`
    borrow that must outlive a guard but is provably scope-bounded). The stack (not a single slot)
    supports the case where a downstream lambda itself starts a NESTED inline drive (e.g. `flatMap`
    over another lazy-spliterator-sourced stream) — always exactly nested, never interleaved, since
    everything here is synchronous single-threaded JVM native execution.
- **`stream_pull_internal`** and **`stream_pull_synthetic_downstream`** (the latter is `flatMap`'s
  inner-stream drive): both now check `stream_lazy_spliterator` first and, if the source is still
  lazy, call `drain_spliterator_inline` instead of `stream_source_elems` + a loop over the resulting
  array. The ordinary (already-materialized / eagerly-sourced) path is completely unchanged.
- **`stream_elements`** (the materializing path used by `collect`/`toArray`/`count`/`sorted`/etc.): no
  longer unconditionally calls `materialize_lazy_stream` before checking for a deferred chain. When a
  chain is present, it routes through `stream_apply_chain_full` (-> `stream_pull` ->
  `stream_pull_internal`), which — per the fix above — drives a still-lazy source inline. Only the
  genuinely chain-less case (e.g. `StreamSupport.stream(sp, false).collect(...)` with no intermediate
  ops at all) still does the plain raw drain, which is correct/unavoidable there: with no downstream
  op to interleave, HotSpot has nothing to key `hasNext()`'s state change on either (verified — see
  below).
- **`native_stream_for_each`**: its pre-existing bespoke "drive the lazy spliterator directly, one
  `tryAdvance` at a time, feed straight to the consumer" fast path is now guarded with
  `!stream_has_chain(ctx, this)`. Before this fix, `stream_make_lazy_derived` never actually left a
  chained stream holding a live spliterator (it always drained first), so this guard was unreachable
  dead code in practice; after this fix, a stream CAN carry both a chain and a still-live spliterator
  (`.map(...).forEach(...)`), and without the guard `forEach` would have hit its own fast path and fed
  the consumer raw, un-mapped elements, skipping the chain entirely. A chained lazy stream now falls
  through to `stream_elements` instead, which is chain-aware.

Total: ~425 lines added / ~30 removed in `native-collections/src/lib.rs`, all within the functions
listed above; no other crate touched.

## Verification

1. **Unit tests**: `cargo test -p cratonvm-native-collections --lib` — 72 passed, 0 failed (same count
   as before the fix; no regressions, no new tests added at this layer since the bug is
   integration-shaped, not unit-shaped).
2. **`cargo check -p cratonvm-native-collections`**: clean except one PRE-EXISTING unrelated warning
   (`materialize_lazy_stream(ctx, this_cur)` unused-`Result` in `stream_source_elems`, present in the
   file before this change too).
3. **The documented minimal repro** (`StreamCursorProbe.java`, verbatim from the referenced doc):
   - Real HotSpot (JDK 25): `result=[a, b, c]` / `PASS`.
   - CratonVM `dev` @ `90cc7e73` (pre-fix, prebuilt binary reused from
     `/data/wt-kc-webdriver-jmx-teardown-20260715`): `Exception in thread "main"
     java/lang/ArrayIndexOutOfBoundsException` at `Cursor.readEntry`, called from the `.map()` lambda —
     matches the doc's predicted failure mode exactly.
   - This fix: `result=[a, b, c]` / `PASS` — byte-identical to HotSpot.
4. **Real-world Selenium JSON path**: `org.openqa.selenium.json.Json.toType(json, Map.class)` against
   `selenium-json-4.41.0.jar` (reused from `/data/wt-kc-webdriver-jmx-teardown-20260715`), both a flat
   2-key object and a larger 10-key object with nested map/list/boolean/null values. Both the pre-fix
   and post-fix binaries decode these correctly. **This specific probe did not discriminate** — i.e. it
   did not reproduce the `JsonException: Unable to determine type from: ','` failure the referenced doc
   predicted, on EITHER binary, at the sizes tried here. The general "cursor" bug is real and
   conclusively fixed (item 3 above, plus the from-scratch regression suite in item 5), but this
   session did not pin down the exact payload shape/size at which `JsonInputIterator`/`MapCoercer`
   actually trips it in practice (see Residual scope).
5. **From-scratch regression suite** (`StreamRegressionSmoke.java`, 27 checks spanning eager
   `Collection.stream()` pipelines — map/filter/collect, sorted (natural + comparator), distinct,
   flatMap, limit/skip, count, reduce, findFirst, anyMatch/allMatch/noneMatch, peek-with-short-circuit,
   forEach, toArray, `IntStream`, and a 100k-element pipeline — plus 9 lazy-cursor-sourced cases
   covering map+collect, map+filter+collect, map+peek+collect (asserting the peek trace), forEach with
   and without a chain, findFirst short-circuit (asserting the cursor stopped mid-drain, not
   exhausted), `flatMap` over a lazy-cursor inner stream, and `sorted()` over a lazy chained source):
   - Real HotSpot: all 27 pass (after fixing one flawed test case that turned out to be a genuinely
     degenerate program on HotSpot too — see note in the source).
   - CratonVM pre-fix: 17/27 pass (all eager cases), then throws `ArrayIndexOutOfBoundsException` on
     the first lazy-cursor case.
   - CratonVM post-fix: 27/27 pass, byte-identical to HotSpot, including the short-circuit assertion
     (`findFirst` over a 5-element lazy cursor stops after reading exactly 2 entries, not all 5).
6. **`git worktree`**: `/data/wt-stream-eager-drain-fix-20260715` (branch
   `fix/stream-eager-drain-20260715`), left in place with a built `target/release/cratonvm` binary and
   the probe/test `.java` files under `/tmp` on the build host, for any follow-up.

## Residual scope (deliberately not touched)

- **Primitive streams** (`IntStream`/`LongStream`/`DoubleStream`): these never build a deferred
  op-chain at all (`int_stream_elements`'s doc comment: "Primitive streams never carry a
  reference-Stream deferred op-chain"). A lazy-spliterator-sourced primitive stream still goes through
  the plain `materialize_lazy_stream` raw drain unconditionally. In practice this is very unlikely to
  hit the same "cursor" bug: the JDK's `Spliterators.spliteratorUnknownSize` for primitives requires an
  `OfInt`/`OfLong`/`OfDouble` iterator returning primitive values (`nextInt()`/`nextLong()`/
  `nextDouble()`), not an object — the "iterator returns `this`" idiom doesn't have an obvious primitive
  analogue. Left unfixed; flagged here in case a real-world repro ever surfaces.
- **The exact Selenium `JsonInputIterator`/`MapCoercer` payload shape/size that reproduces the original
  `JsonException`** was not pinned down (see verification item 4). The architectural bug is fixed and
  conclusively demonstrated via the documented minimal repro and the broader regression suite; a
  dedicated follow-up could bisect payload size/nesting to find where the real Selenium code path
  actually engages the (now-fixed) lazy-spliterator machinery, if full end-to-end confidence via the
  real library is wanted.
- **The unrelated zipfs `Files.copy` bug** (item 3 in the referenced doc, tracked separately as
  task_eccb7c4c) was not touched by this session — out of scope, per the referenced doc. It was,
  however, independently fixed and closed by a concurrent session on this same shared host while
  this fix was in progress: see `90cc7e73` ("merge: fix zipfs Files.copy source-path bug into dev")
  and `896a7da5` ("docs(known-issues): close zipfs Files.copy bug FIXED, teardown hang re-verified
  not reproducing" -> `docs/internal/fixed-suite-bugs/zipfs-files-copy-wrapped-path-FIXED.md`), both
  now ancestors of `dev`. That doc also re-verified the originally-reported `WelcomePageTest`
  teardown hang (item 4) as no longer reproducing. Full `WelcomePageTest` end-to-end re-verification
  specifically exercising THIS fix (rather than the standalone JSON-parsing checks above) was not
  attempted here and remains open follow-up scope if desired.
- **`Stream.iterate`/`Stream.generate`-style infinite lazy streams** consumed only through `limit()`
  were not specifically exercised beyond the `limit`/`skip` chain-op coverage already in the regression
  suite (those apply to array-backed sources in the suite, not a genuinely infinite spliterator). The
  chain-level `stream_limit_saturated` short-circuit logic is unchanged by this fix (still the same
  code, just now also reachable from the inline spliterator-driven path), so no regression is expected,
  but a genuinely-infinite lazy spliterator + `limit()` combination was not independently verified.

## Merge

Merged to `dev` from `fix/stream-eager-drain-20260715` (based on `origin/dev` @ `b658c5e8`).
