# Scoping: a debug-build assertion for stale native-local `ObjectRef` reads

Status: SCOPED, not implemented. Written 2026-07-11 during the static-analysis sweep
([[wildfly-parallel-boot-stale-objectref-residual]]) as the lower-priority second half of that
session's task. Explicitly deferred — this is a GC/heap-level change, larger and riskier than the sweep
itself, and warrants its own dedicated session with real validation budget.

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

## Sketch: heap-side quarantine + tombstone, checked at the `NativeContext` boundary

This mirrors how AddressSanitizer / Valgrind catch use-after-free, adapted to a moving (not freeing) GC:

1. **Quarantine evacuated from-space regions for one extra GC cycle** (debug/`CRATONVM_DBG_*`-gated
   only). Instead of immediately making a just-evacuated region available for new allocations, keep it
   mapped but unused until *after* the collector completes its *next* cycle. This costs extra address
   space and one cycle's worth of memory pressure — acceptable for debug builds and CI, not for release.
2. **Write a tombstone header into every evacuated object's old location** before it's reused (or, given
   the quarantine above, at evacuation time, since the memory won't be reused until the tombstone has had
   a chance to be observed). A tombstone needs: a recognizable magic value distinguishable from any real
   object header, and the forwarding address (where the object now lives) for a useful panic message.
3. **Check at the `NativeContext` trait boundary**, not at every Rust pointer dereference. Every method
   that takes an `ObjectRef` argument (`get_field`, `set_field`, `invoke*`, `array_length`,
   `identity_hash_code`, etc. — essentially all of them) is exactly the boundary where a native function
   hands a possibly-stale local back to the VM. Add a `#[cfg(debug_assertions)]` (or env-var gated, to
   also run in release-with-debug-assertions CI configurations) check at entry: read the object header at
   the given address; if it's a tombstone, panic immediately with the forwarding address and (if
   feasible) a captured Rust backtrace, instead of silently proceeding to read/write whatever now occupies
   that slot.
4. **Do not check `pin_native_root`/`read_native_pin`/`unpin_native_roots` themselves** — those are the
   sanctioned path and already correctly re-read the forwarded address every time.

### Why the boundary check, not a blanket check

A real per-dereference check (e.g. in the GC's own `get_field`/`set_field` primitives, which native code
and interpreted bytecode both funnel through) would also catch legitimate interpreter-side reads that
haven't gone through `NativeContext` at all, and might be redundant with existing interpreter-side root
tracking. Scoping the check to the `NativeContext` trait impl (the real-VM one in `vm/src/vm/vm_exec.rs`,
around the existing `pin_native_root`/`read_native_pin`/`unpin_native_roots` methods at line ~2516) keeps
the blast radius to exactly the boundary this whole bug class crosses, and keeps the change local to one
file rather than threading a check through the entire interpreter.

## Open questions / risks for whoever picks this up

- **Where does "evacuation" currently happen, precisely?** Needs a `gc` crate read to find the exact
  evacuation/copy step to hook the tombstone-write into, and to confirm regions are asynchronously
  reused (vs. always immediately reused) so the quarantine delay is actually implementable without a
  bigger refactor.
- **Multi-threaded safety**: `native_pin_roots` is per-thread already; a tombstone check must not race
  with a concurrent GC cycle re-evacuating the SAME already-quarantined region a second time before the
  quarantine window elapses — needs a clear invariant (e.g. quarantine survives exactly one full cycle,
  never re-evacuated while quarantined).
- **Perf in debug/CI builds**: quarantining doubles (at least) the working set for evacuated regions
  during the window; likely fine for `cargo test`/CI but needs a measurement, not an assumption.
- **False negatives are still possible**: if a stale read happens to land in memory that was quarantined
  and already recycled past the window (e.g. a very long-lived stale reference held across many GC
  cycles, past a single-cycle quarantine), the tombstone will already be gone and the read will silently
  succeed against whatever new object occupies it — same failure mode as today, just a narrower window.
  A longer quarantine narrows this further at a higher memory cost; there's a real tradeoff here, not a
  free lunch.
- **Alternative, cheaper partial mitigation** if the above proves too invasive: instrument only
  `pin_native_root`'s *absence* — i.e., statically enforce (via the sweep's scanner, turned into a real
  lint/CI check rather than a one-off script) that no new PR introduces the pattern, without any runtime
  cost at all. Weaker (catches at review time, not at every actual stale-read event, and only for
  patterns the static tool's heuristics recognize) but zero runtime cost and immediately actionable —
  worth doing regardless of whether the heap-side assertion above is ever built.

## Recommendation

Don't attempt the full heap-side assertion without a session budgeted for real GC-code spelunking and
validation (this is comparable in risk profile to the other explicitly-deferred GC-barrier work in
[[wildfly-parallel-boot-stale-objectref-residual]]'s STW-stall section). In the meantime, the cheaper
"turn the sweep's scanner into a checked-in CI lint" alternative above captures most of the value
(stopping *new* instances of this exact pattern from landing) for a fraction of the risk and effort.
