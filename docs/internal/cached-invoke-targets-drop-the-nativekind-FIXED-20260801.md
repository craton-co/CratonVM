# `CachedInvokeTarget` stores a native callback without its `NativeKind`, so a cache *hit* cannot re-apply the dispatch policy

**Status:** FIXED and retired on 2026-08-01.

## Resolution

`CachedInvokeTarget::Native` and `::VirtualNative` now retain both the stable
`NativeMethodId` and its `NativeKind`. Every construction site resolves that
identity once. On a compatible-mode cache hit, the current callback and kind
are read back by id, preserving last-registration-wins semantics. On a
JDK-only hit, the cached native is revalidated through the central
`resolve_native_dispatch_wave1` policy. The existing `RedefineGate` is checked
first and evicts an entry if bytecode availability may have changed.

Both warm arms now call `record_invocation(id)`. This removes the old static
constant-pool/name re-resolution and triple hash, closes the previously
uncounted virtual path, and removes the virtual hit path's
`real_protected_stub_class` pre-filter.

Validation used the final release binary
`cratonvm-tomcat-charsetcache-r13-20260801-019fb049` (SHA-256
`65e66e82ff44a3e27f51fc70dcfac10ccf19a208b4b34591b50f47f5460d7900`):

* `cratonvm-vm --test jdk_only_dispatch`: 12/12 passed.
* A warm-cache JDK-only probe passed in both JIT and `--nojit` modes and
  produced identical non-vacuous schema-2 census totals: 300,053 bridge
  invocations, one intrinsic invocation, and zero synthetic-stub invocations.
* Each report attributed exactly 100,000 calls to virtual
  `Runtime.freeMemory`, virtual `Thread.isAlive`, and static `System.nanoTime`,
  proving that both cached native shapes are counted rather than merely that
  the aggregate counter is non-zero.
* The exact Tomcat `TestCharsetCachePerformance` class passed under JIT
  (`OK (1 test)`), while a 1,000,000-lookup charset semantic stress passed in
  both JIT and `--nojit` modes.

The rest of this record is retained as the pre-fix analysis.

## What is wrong

`classloading/src/resolution.rs`, `enum CachedInvokeTarget<JitMethod = ()>`
(line 1223). The two native arms are at 1232 and 1249:

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

(The sibling `Intrinsic` arm at 1289 *does* carry a `kind`, but it is
`cratonvm_native_api::InterpIntrinsic` — the interpreter intrinsic-table
identity, kept "for the hit counter, the on/off debug flag, and `Debug`
formatting". It is a different enum and answers a different question.)

`RedefineGate` shows that the "re-check on hit" problem was already understood
in another dimension: a redefine that swaps a native body for bytecode must
evict the entry, so a generation snapshot is carried and checked in O(1) on
every hit. The dispatch *policy* got no equivalent.

The fill site says so itself —
`vm/src/runtime/interpreter/invoke.rs` ~12723, in `populate_invoke_cache`:

> **JDK-ONLY-WAVE2:** `CachedInvokeTarget::Native` stores the callback and
> throws the `NativeKind` away. A cache HIT therefore cannot re-ask the §7
> question — it has no idea whether it is about to run a reviewed intrinsic or a
> `SyntheticStub` — which is why `execute_invokestatic_cached` has to re-derive
> the triple from the constant pool just to run the stub-yield gate. … That
> variant lives in `classloading/src/resolution.rs`, outside this wave's file
> ownership.

## What the hit path actually does today

`vm/src/runtime/interpreter/invoke.rs` ~22243, on a `VirtualNative` hit,
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

## The counting half — half-closed by the re-land

The original filing said cached dispatches are entirely invisible to the census.
That is now true of one of the two arms and not the other.

**Static (`CachedInvokeTarget::Native`) — counted.**
`execute_invokestatic_cached` (`invoke.rs` ~13013) piggybacks on the
constant-pool resolution the stub-yield gate had already paid for:

```rust
// §4 census ONLY — the dispatch decision was taken when this entry
// was cached and is not revisited here … without it, every warmed
// static native call site would be invisible to the zero-stub
// acceptance criterion. This does not change what runs.
crate::vm::record_native_dispatch(shared, &class_name, &method_name, &descriptor);
```

**Virtual (`CachedInvokeTarget::VirtualNative`) — deliberately uncounted**, and
this is the high-volume arm. From its own marker (`invoke.rs` ~13016):

> the cached *virtual* native path … is deliberately left UNCOUNTED. It has no
> equivalent already-paid constant-pool resolution to piggyback on, so a census
> increment there would add a full triple hash to the hottest warmed dispatch in
> the interpreter for a measurement-only feature. What must replace it: the
> `kind`+id carried on the cached target (same marker as above), which makes the
> increment a relaxed add and the hash unnecessary.

So contract §11's acceptance criterion — *"zero synthetic-stub invocations
through any path"* — is verifiable for warmed **static** natives and still
unverifiable for warmed **virtual** ones.

## The enabling accessors now exist

The original filing asked for a `find_id_with_kind`. The re-land landed the
same capability under different names, in `native-api/src/registry.rs`:

* `resolve_id(class, method, desc) -> Option<NativeMethodId>` (~5445)
* `callback_of(id)` (~5486), `kind_of_id(id)` (~5492)
* `record_invocation(id)` (~5517), `invocations_of_kind(kind)` (~5552)

`find_with_kind` (~5383) is untouched, as contract §4 requires. `vm/src/jit/helpers.rs`'s
`admit_jit_fast_native` (~6095) already uses exactly this
`resolve_id` + `kind_of_id` + `record_invocation` shape and is the worked
example to copy. **Nothing blocks step 1 and 2 below any more.**

> **Evidence provenance.** All line numbers above were read from
> `C:\craton\wt-jdk-only` (branch `feat/jdk-only-mode`) on 2026-07-31, after the
> wave-1 re-land. The `CachedInvokeTarget` definition and the hit-path code are
> pre-existing and unchanged by the re-land; the markers, the static-path
> census increment and the `resolve_id` family are re-landed code.

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
   `record_invocation` with no second lookup. `resolve_id` / `kind_of_id` /
   `callback_of` already provide it.
3. Replace the name-based re-derivation on the hit path with a direct check
   against the stored kind, and **delete the `real_protected_stub_class`
   pre-filter** — it becomes both unnecessary and wrong once the check is cheap.
4. Drop the static path's `record_native_dispatch` triple hash in favour of the
   stored id; it is a stopgap that exists only because the id is missing.
5. Route the hit path through `resolve_dispatch` (contract §7) like the cold
   path, so there is one decision function rather than two.

## How to verify a fix

* **Parity, cold vs warm:** for a corpus of `(receiver class, method,
  descriptor)` triples, assert that the verdict served on a cache *hit* equals
  the verdict `resolve_dispatch` produces cold. Any difference is the bug.
* **Coverage:** with the id stored, `--dump-native-registry` (schema 2)
  `invocations` must become non-zero for natives that are known to be reached
  only through warm **virtual** call sites. Static ones are already non-zero, so
  a column that is non-zero overall proves nothing on its own — compare the two
  arms.
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
* [The forced-native `String` policy](forced-native-string-policy-two-lists-that-disagree.md)
  — the same cold-path/warm-path split, with a worse outcome.
* [Additional wave-2 markers §1](additional-wave2-markers-not-in-the-original-inventory.md)
  — the JIT's MIC and PIC slots have the same missing-kind shape. The re-land
  bought time there by refusing to publish native entries under `JdkOnly`;
  fix both together or the JIT's refusal becomes permanent.
