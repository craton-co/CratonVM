# Debug-build assertion for stale native-local `ObjectRef` reads

Status: **IMPLEMENTED for the `Generational` backend** (`CRATONVM_DBG_STALE_OBJREF=1`), same session,
after initially being scoped-but-deferred. See "Implementation" below for what actually landed, and
"Not covered" for the explicit scope boundaries (G1, ZGC, and a couple of narrower gaps). Originally
written 2026-07-11 during the static-analysis sweep ([[wildfly-parallel-boot-stale-objectref-residual]])
as a design sketch only; the user then asked for the full (quarantine + tombstone) version to be built
in-session rather than the cheaper CI-lint alternative this doc's "Recommendation" section had suggested
as a stopgap.

## The bug class this targets

Every site the sweep found (and every site fixed in the 3 sessions before it) has the same shape: a
native Rust function reads an `ObjectRef` (from an arg, a `get_field`, or a `ctx.*` allocator), then makes
one or more calls that can trigger a moving GC (`ctx.invoke*`, `ctx.alloc_object`, `ctx.new_array`,
`ctx.create_string`, `ctx.ensure_class_initialized`, etc.), then uses the *original* Rust local again. Per
`pin_native_root`'s own documented contract (`native-api/src/registry.rs`), a raw `ObjectRef` copied out of
the heap into a native local is **not** a GC root — only `Value`s live on the interpreter's own operand
stack/locals, or objects reachable through the normal object graph, get relocated automatically. A raw
native local silently keeps pointing at the OLD address, which the moving collector may have already
handed to something else (typically resolving to a bare `java.lang.Object` header — hence
`"Constructor.newInstance: no declaring class"` and its many siblings).

The existing mitigation (`pin_native_root`/`read_native_pin`/`unpin_native_roots`, a per-thread
`Vec<ObjectRef>` the GC treats as real roots and updates in place — see `vm/src/vm/vm_exec.rs`) works, but
is opt-in and easy to forget: nothing stops a new native function (or an old one, still un-audited) from
introducing another instance of the exact same bug. The static-analysis sweep is a **point-in-time**
mitigation; it doesn't stop the next occurrence from being written. A debug-build assertion would.

## Goal

Convert this bug class from "silent corruption, 1-20% of the time, depending on GC timing" into "always
caught, deterministically, in CI/tests, the first time it happens" — without materially slowing down
release builds.

## Why it can't just be "tag the `ObjectRef` value"

`ObjectRef` (`types/src/value.rs`) is `#[derive(Copy)] struct ObjectRef { ptr: NonNull<u8> }` — a bare
pointer, Copy, used pervasively (millions of call sites, FFI-adjacent, `Value::Object(Option<ObjectRef>)`
relies on its niche optimization for a tag-free `Option`). Adding a side-channel "epoch this was captured
at" field to the struct itself would:
- Break the `Option<ObjectRef>` niche optimization (no longer pointer-sized).
- Require every one of those call sites to thread the extra field through.
- Not actually detect the bug anyway — the epoch would need to be compared against something *external*
  (was a GC cycle observed between capture and use?), which the plain value alone can't encode without a
  side table keyed by identity, at which point the pointer itself carries no useful information.

The bug is really about **the pointer's target memory**, not the pointer value: has this address been
evacuated by the collector since this code last legitimately read it? That's a property of the heap, not
of the 8-byte value copied around in Rust locals.

## Implementation (what actually landed)

Turned out to need less new machinery than the original sketch below assumed: the `Generational` heap
backend (`gc/src/gen_heap.rs`) already installs a real forwarding pointer at an evacuated object's OLD
address during every minor GC (`forward_object_impl`, "Install forwarding pointer in the old header"),
and `ObjectHeader::is_forwarded()`/`forwarding_address()` (`types/src/heap_types.rs`) already exist to
read it. The only reason that signal isn't usable *outside* the GC pause today is that
`young_from.reset()` immediately zeroes the evacuated arena (including every forwarding pointer in it)
right after the pause, before mutators resume — so no *new* tombstone format was needed, just a way to
defer that one zero-fill by one cycle.

**Mechanism** — `GenerationalHeap` gained a third arena, `quarantine: Mutex<Arena>`, starting at zero
capacity (so it costs nothing when the flag is off). When `CRATONVM_DBG_STALE_OBJREF` is set,
`collect_garbage_inner`'s `young_from.reset()` call is replaced with: reset whatever was quarantined
*last* cycle (its one-cycle grace period has elapsed), grow it to match `young_from`'s current capacity
if `young_from` has since expanded, then `swap` it with `young_from` — so `young_from` becomes the
freshly-reset arena (identical external effect to the plain reset) and `quarantine` becomes THIS cycle's
just-evacuated, still-intact garbage. A stale native `ObjectRef` pointing into it still has a valid
`is_forwarded()`/`forwarding_address()` for one full extra minor-GC cycle. [`GenerationalHeap::get_header`]
— the single low-level accessor every `NativeContext` field/array/class-id method funnels through (`get_field`,
`set_field`, `class_id_of`, `array_length`, `identity_hash_code`, etc. — confirmed by grepping every call
site in `gen_heap.rs`) — panics if `CRATONVM_DBG_STALE_OBJREF` is set and the header it just read is
forwarded. The GC's own internal forward/remap/scan code never calls `get_header()` (it reads headers via
its own raw `addr_of!`/pointer-cast logic in `forward_object_impl`, and the separate self-healing
`VmHeap::load_and_forward` barrier does its own raw cast too), so the check cannot false-positive on
legitimate GC-internal header reads — confirmed by grep, not just reasoning about it.

Files: `gc/src/stale_objref_debug.rs` (new — the cached env-gate, mirroring `gc/src/a2dbg.rs`'s pattern),
`gc/src/gen_heap.rs` (the `quarantine` field + `get_header` check + the `collect_garbage_inner` swap
dance), `gc/tests/stale_objref_debug_assertion.rs` (new — a single-test integration binary; see its own
doc comment for why it has to be a dedicated file, not a test inside `gen_heap.rs`'s existing module: the
env-gate's `OnceLock` latches permanently on first read, and `cargo test` runs every test in one file in
the same process).

**Verified**: the new integration test (allocate → GC → assert the survivor reads back correctly →
`catch_unwind` the stale OLD local and assert it panics with the right message → a second GC completes
normally) passes. The full `cratonvm-gc` crate test suite (823 tests: unit + `leak_soak` + `proptest_graph`
+ `wp1_10_reference` + doc-tests) passes unchanged with the flag off, confirming zero behavioral change
to the default (flag-unset) path.

## Not covered (explicit scope boundaries)

- **G1 and ZGC backends are untouched.** G1 has two evacuation paths (a parallel CAS-based one and a
  serial one — `gc/src/g1.rs`) plus a region-based CSet/recycling model quite different from the
  semispace young-gen this targets; ZGC is fully concurrent. Extending this same idea to either is real
  additional work with its own risk profile, not a copy-paste of the Generational change. `Generational`
  is the workspace default (`vm/src/config.rs`'s `GcAlgorithm::default()`), so this covers ordinary
  `cargo test`/`cratonvm` runs without `--gc g1`/`--gc zgc`, but not those explicit modes.
- **Doesn't cover `VmHeap::load_and_forward`'s own callers.** A handful of specific call sites
  (`vm/src/runtime/interpreter.rs`, `vm/src/vm/vm_exec.rs` — e.g. `invoke_virtual` receiver resolution)
  call this self-healing barrier directly, which does its own raw header read and transparently resolves
  a forwarded reference *without* going through `get_header()`. A stale reference reaching exactly one of
  these specific call sites (as opposed to a `get_field`/`set_field`/etc. call) would be silently healed
  rather than caught — arguably correct behavior for those call sites' own purpose, but it does mean this
  assertion's coverage isn't 100% of every possible stale-read path.
- **Doesn't cover the JIT's guarded-inline `getfield` fast path.** JIT-compiled code can bypass
  `get_header()` entirely for addresses inside the published young-from/young-to/old-gen bounds
  (`JIT_REGION_BOUNDS`, `gen_heap.rs`) via an inlined raw pointer load. The quarantine arena here is
  deliberately never published into those bounds, so a stale reference into it correctly falls through to
  the checked slow-path helper instead of the fast path — but that checked helper's own internals were not
  audited as part of this change, so whether it also catches the stale case is unverified. This limitation
  is orthogonal to (not a regression from) this change: it's the same "register-invisible JIT root" bug
  family already tracked elsewhere as its own open issue.
- **One-cycle-only quarantine.** A stale reference that survives *two or more* GC cycles unread will find
  its quarantine window already closed and its memory already reclaimed/reused — the read then silently
  succeeds against whatever now occupies that slot, exactly like the pre-existing (unfixed) behavior. This
  was always a known tradeoff (see "Open questions" below) and wasn't revisited in the implementation
  session — every fix in this whole investigation's history involves the object going stale across
  exactly ONE intervening GC-triggering call, so a one-cycle window covers the actual observed bug shape.

## Open questions / risks for extending this further

- **Multi-threaded safety**: this implementation only exercises the single-threaded-STW case (matching
  every existing `gen_heap.rs` test's `StopTheWorldToken` usage). The `quarantine` field is protected by
  its own `Mutex` exactly like `young_from`/`young_to`, so cross-thread correctness should follow the same
  invariants as the existing swap — but this was not specifically stress-tested under concurrent mutators
  racing a collection.
- **Perf in debug/CI builds**: the quarantine arena roughly doubles young-gen memory footprint for the
  *evacuated* region's lifetime when the flag is on (never when it's off — capacity starts at 0 and only
  grows on first use). Not measured under real allocation pressure; likely fine for `cargo test`/CI given
  young semi-spaces are already small, but not verified against a large heap / high-allocation-rate
  workload.
- **Extending to G1/ZGC** remains real, separately-scoped work — see "Not covered" above.
- **The cheaper "turn the sweep's scanner into a CI lint" alternative** (statically enforcing the
  unpinned-`ObjectRef`-across-GC-call pattern is absent from new PRs, zero runtime cost) is still worth
  doing independently of this runtime assertion — the two are complementary (one catches new code at
  review time, the other catches any code, old or new, syntactically-hidden or not, at actual
  corruption time) — but wasn't built in this session.
