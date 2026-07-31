# `CachedInvokeTarget` stores a native callback without its `NativeKind`, so a cache *hit* cannot re-apply the dispatch policy

**Status:** OPEN — JDK-only wave-2 work item, filed 2026-07-31. **DANGEROUS:
the policy is silently applied on the cold path and only partially on the warm
path.** This is the structural reason CratonVM keeps two divergent copies of
every native-vs-bytecode decision.

## What is wrong

`classloading/src/resolution.rs`, `enum CachedInvokeTarget<JitMethod = ()>`
(~line 1223). The two native arms are:

```rust
/// Native method: direct function pointer (invokestatic/invokespecial).
Native {
    callback: NativeCallback,
    num_params: u16,
    gate: RedefineGate,
},
…
/// Monomorphic virtual native: checks receiver class, dispatches if match.
VirtualNative {
    receiver_class_id: ClassId,
    callback: NativeCallback,
    num_params: u16,
    gate: RedefineGate,
},
```

No `NativeKind`. Once the entry is populated, the information that decides
whether this native may run at all under `CompatibilityMode::JdkOnly` —
`Intrinsic` (always allowed), `Bridge` (allowed), `SyntheticStub` (forbidden) —
is gone.

(The sibling `Intrinsic` arm *does* carry a `kind`, but it is
`cratonvm_native_api::InterpIntrinsic` — the interpreter intrinsic-table
identity, kept "for the hit counter, the on/off debug flag, and `Debug`
formatting". It is a different enum and answers a different question.)

`RedefineGate` shows that the "re-check on hit" problem was already understood
in another dimension: a redefine that swaps a native body for bytecode must
evict the entry, so a generation snapshot is carried and checked in O(1) on
every hit. The dispatch *policy* got no equivalent.

## What the hit path actually does today

`vm/src/runtime/interpreter/invoke.rs` (~21890), on a `VirtualNative` hit,
re-derives the verdict from names because the entry cannot supply it:

```rust
let receiver_name = shared.classes.class_manager.read()
    .get_class(actual_class_id).map(|class| class.name.clone());
if let Some(receiver_name) = receiver_name.filter(|n| real_protected_stub_class(n)) {
    if let Ok((_owner, method_name, descriptor, _)) =
        resolve_method_ref(shared, caller_class_id, cp_index)
    {
        if synthetic_stub_should_yield_to_real_bytecode(
            shared, &receiver_name, &method_name, &descriptor,
        ) { …evict, CacheMiss… }
    }
}
```

Two things follow, and both matter.

**1. The re-check is gated on a hard-coded allow-list.** The receiver-name
pre-filter is `real_protected_stub_class(n)`. For any class *not* on that
list — which is almost every class — the cached callback is served with **no
policy check at all**. Under `Compatible` that is correct by construction
(every kind yields the same callback). Under `JdkOnly` it means a cached
`SyntheticStub` callback for a non-allow-listed class runs unchecked, forever,
at that call site.

**2. The pre-filter exists purely for throughput, and the comment says so.**

> PERF (H2 `TestFileSystem.testConcurrent`, 2026-07-26): this used to run
> `resolve_method_ref` — a resolution-cache `RwLock` read, a hash probe and four
> `Arc` clone/drop pairs — on EVERY cached virtual-native call, before
> discovering (as it almost always does) that the receiver is not a
> real-protected stub class at all. … Test that term first, so the expensive
> half runs only for the handful of classes (`ReentrantLock`, `EnumSet`,
> `Instant`, …) that can actually yield.

That is a sound optimisation of the *existing* predicate. It is also a direct
consequence of the missing field: if the entry carried its `NativeKind`, the
whole re-derivation — lock, hash, `Arc` traffic and all — collapses to reading
one byte off the cache entry, and the allow-list gate becomes unnecessary rather
than load-bearing.

## The counting half: cached dispatches are invisible to the census

Contract §4's `record_invocation(id: NativeMethodId)` keys on a slot index that
only `NativeMethodRegistry` issues. A cached entry holds a raw `NativeCallback`,
not an id, so it cannot record. Wave 1's own note on `record_invocation` states
the deliberate scope:

> Paths that only ever call `find`/`find_with_kind` by name have no id to record
> and are deliberately left uncounted rather than given a second lookup —
> resolve to an id first if their volume matters.

The invoke cache is precisely such a path, and it is the *high-volume* one. That
makes contract §11's acceptance criterion — *"zero synthetic-stub invocations
through any path"* — unverifiable through the warm dispatch path, which is where
almost all invocations happen.

> **Evidence provenance.** The `record_invocation` note and the wave-2 markers
> on both cached-native sites were read from the working tree of
> `C:\craton\cratonvm` (branch `dev`, HEAD `0c54a9184`) on 2026-07-31; those
> uncommitted edits to `native-api/src/registry.rs` and `invoke.rs` were
> subsequently reverted — see the *Wave-1 revert* note in [`README.md`](README.md).
> **The `CachedInvokeTarget` definition and the hit-path code quoted above are
> pre-existing and re-verified against the current tree.**

## Why it was not fixed in wave 1

`CachedInvokeTarget` lives in `classloading`, is generic over the JIT's method
type, and is consumed by the interpreter, the JIT and the reflection paths —
three agents' files. Adding a field changes every construction site. Wave 1's
contract for agent E was to land `resolve_dispatch` and route the *main
interpreter path* through it with `Compatible` behaviour preserved bit-for-bit;
widening the cache entry was out of scope.

## What specifically must change

1. Add `kind: NativeKind` to `CachedInvokeTarget::Native` and
   `::VirtualNative`, populated at IC-fill time (the populating code already
   calls `kind_of` / `find_with_kind`, so the value is in hand — it is discarded).
2. Add `id: NativeMethodId` alongside it, so the hit path can call
   `record_invocation` with no second lookup. Contract §4 already anticipates
   the enabling accessor: a `find_id_with_kind(class, method, desc) ->
   Option<(NativeMethodId, NativeCallback, NativeKind)>` — `find_with_kind`'s
   body returning the slot index it already has, leaving `find_with_kind`'s own
   signature and semantics untouched (additive).
3. Replace the name-based re-derivation on the hit path with a direct check
   against the stored kind, and **delete the `real_protected_stub_class`
   pre-filter** — it becomes both unnecessary and wrong once the check is cheap.
4. Route the hit path through `resolve_dispatch` (contract §7) like the cold
   path, so there is one decision function rather than two.

## How to verify a fix

* **Parity, cold vs warm:** for a corpus of `(receiver class, method,
  descriptor)` triples, assert that the verdict served on a cache *hit* equals
  the verdict `resolve_dispatch` produces cold. Any difference is the bug.
* **Coverage:** with the id stored, `--dump-native-registry` (schema 2)
  `invocations` must become non-zero for natives that are known to be reached
  only through warm call sites. A column of zeros where a workload obviously
  dispatched means the wiring is still missing.
* **Throughput regression guard:** `H2 TestFileSystem.testConcurrent` is the
  named workload that motivated the pre-filter; it is the right A/B for the
  claim that a stored kind is cheaper than the pre-filtered re-derivation.
* Under `--jdk-only`, `counts.synthetic_stub_invocations` must be `0` **and**
  `bridge_invocations` + `intrinsic_invocations` must be plausibly large — a
  zero-stub report next to near-zero total invocations means the counting is
  broken, not that the run was clean.

## Blast radius if done wrong

* Populating the kind at IC-fill time but never re-checking the **redefine**
  gate against it reintroduces the exact bug `RedefineGate` exists to prevent,
  one layer up: a class redefined from native to bytecode would keep serving the
  cached kind.
* Removing the `real_protected_stub_class` pre-filter *before* the stored kind
  is in place puts a `resolve_method_ref` back on every cached virtual-native
  call — the 2026-07-26 H2 throughput regression, restored.
* Storing the kind but leaving the cold path's hard-coded lists in place means
  three copies of the policy instead of two.

## Related

* [Two divergent real-protected-stub allow-lists](real-protected-stub-allowlists-diverge.md)
  — the list this hit path pre-filters on.
* [The forced-native `String` policy exists twice](forced-native-string-policy-two-lists-that-disagree.md)
  — the same cold-path/warm-path split, with a worse outcome.
