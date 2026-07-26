# Native-dispatch memoization

Status: **landed in `native-api` (2026-07-26). Consumer adoption in
`vm/src/runtime/interpreter.rs` + `vm/src/vm/vm_exec.rs` is the next wave.**

Slug: `native-dispatch-memoization`. Owner crate: `native-api` only.

## Basis — read this before trusting any line number

Every file:line citation below was re-derived on:

* `dev` = **`6495a191c`** ("Merge remote-tracking branch 'origin/dev' into dev"),
  merged into this branch as `3e31dffc5`.

The first pass of this work was done against `e4e4053bb` (= `origin/main`,
2026-07-23), which was the wrong tree — 114 files / ~25,900 insertions behind
`dev`. `vm/src/runtime/interpreter.rs` alone drifted **+3,919 lines**, so the
original census line numbers were all wrong and are not preserved here. `dev`
is advanced by concurrent sessions; re-verify before adopting if `dev` has
moved again.

Two structural changes on `dev` that the consumer spec depends on:

1. **The registry moved.** `SharedVm.native_methods` is now
   `SharedVm.natives.native_methods` — `vm/src/vm/realms/native_realm.rs`
   introduces `NativeRealm`, which owns the registry **by value** (its own doc:
   "Native method registry (immutable after construction)"). Every code snippet
   below uses `&shared.natives.native_methods`.
2. **`CachedBytecodeMethod` gained a hand-written `Clone`** (it no longer
   derives it) and three new memo fields — see §3 Step 0.

## Prior art check

The memoization described here **does not already exist on `dev`**. As of
`6495a191c`, `NativeMethodRegistry` still carries the two parallel digest maps
(`methods: FxHashMap<(u64,u64), NativeCallback>` and `category_by_key:
FxHashMap<(u64,u64), NativeKind>`), and `find` still re-hashes all three
strings per call. `dev`'s concurrent additions to `native-api/src/registry.rs`
(+300 lines) were `NativeHandle` / `NativeHandleScope` (GC-root scoping for
natives — unrelated), several new defaulted `NativeContext` methods, a
known-issues doc-path fix, and a rustfmt reflow of `find_with_kind`. Only the
last touched code this change rewrites, and it was formatting only.

---

## 1. The finding, verified against dev

CratonVM dispatches native methods by hashing the triple
`(class_name, method_name, descriptor)` into a 128-bit key (two independent
FNV-style passes plus splitmix finalization — `registry::native_method_hash`)
and probing an `FxHashMap`. ~3,100 natives are registered at boot in synthetic
mode; ~300 in real-JDK mode, but those sit on the hottest paths
(`System.arraycopy`, `Object.hashCode`, string/array intrinsics).

`NativeMethodRegistry::find` re-hashes all three strings on **every call**. That
is not an O(1)-vs-O(n) problem, it is a constant-factor problem: the hash is a
byte-at-a-time walk over three strings whose combined length is routinely
60-150 bytes, because JVM descriptors are long. Two independent profiling
sessions already caught it, and each was fixed locally with its own bespoke
mechanism:

| Profile | Finding | Local fix |
|---|---|---|
| `perf` on `TestResponsePerformance` | `NativeMethodRegistry::find` at ~7% of samples — #2 hottest symbol behind the interpreter's frame-dispatch loop | `CachedBytecodeMethod::native_callback_cache` (`jit-api/src/lib.rs:86`), a `OnceLock<Option<NativeCallback>>` |
| `gdb` sampling of a hung-looking H2 `TestFileSystem.testConcurrent` | both live threads repeatedly inside `hash_byte_pair` / `native_method_hash` | `NativeMethodRegistry::find_with_kind`, folding a redundant second `kind_of` hash into the first probe |

### 1.1 Census of `find` call sites (dev @ 6495a191c)

Measured by a multiline scan for `native_methods.{find,find_with_kind,
find_by_method_descriptor}(`. Two methodology notes, both of which change the
answer:

* A **line-oriented grep undercounts by ~4x**. Most call sites are rustfmt-
  wrapped across several lines (`shared` / `.natives` / `.native_methods` /
  `.find(`). The primary `try_stackless_invoke` lookup — the single most
  important site in the file — is one a line grep misses.
* Sites inside `#[cfg(test)]` modules are excluded, and matches that fall
  inside comments are excluded (e.g. `vm_exec.rs:17421`, which quotes
  `native_methods.find(class_name, ...)` in prose).

| File | Production sites | Test-only |
|---|---|---|
| `vm/src/runtime/interpreter.rs` | **64** | 0 (test mods start at 3568, 12650, 42392; highest prod site 41638) |
| `vm/src/vm/vm_exec.rs` | **40** | 0 (test mods at 1575, 19673; highest prod site 19518) |
| `vm/src/jit/helpers.rs` | 2 | 0 |
| `vm/src/vm/vm_util.rs` | 1 | 0 |
| `vm/src/vm/vm_object.rs` | 1 | 0 |
| `vm/src/vm.rs` | 0 | 35 (all inside `#[test]` fns) |
| `vm/src/vm/vm_init.rs` | 0 | 33 (all past the `#[cfg(test)] mod tests` at 5624) |
| **Total production** | **108** | — |

**Memoized on dev today: one mechanism, covering 3 sites.**
`CachedBytecodeMethod::native_callback_cache` is read at
`interpreter.rs:29913` (`intercept_force_registered_native_cached`),
`interpreter.rs:39215` (the vtable-hit force-native gate — added on `dev` since
the original survey) and `interpreter.rs:40054` (the JIT virtual-tier-up
`has_registered_native` guard). Nothing else caches anything.

So ARCHITECTURE.md's "several interpreter sites still re-resolve per
invocation" is **true but understates the situation by an order of magnitude**:
**61 of 64** production interpreter sites re-resolve on every invocation, as do
**40 of 40** in `vm_exec.rs`. The accurate statement is "three interpreter
sites share one memo; every other native lookup in the VM re-hashes three
strings per call".

### 1.2 Classification

Classifying the 104 `interpreter.rs` + `vm_exec.rs` sites by *what determines
the triple being looked up* (bucket totals ±1 — the constant-vs-callsite
boundary is a judgement call on a few sites; the line lists are exact):

**(a) Already memoized — 3 sites.** `interpreter.rs:29913`, `:39215`, `:40054`
(all one `native_callback_cache`).

**(b) Constant class name — ~37 sites.** Of these, **8 are fully-constant
triples**: all three arguments are string literals, so the lookup is a
compile-time constant recomputed on every call.

```
interpreter.rs:29618  find("java/lang/Class", "getClassLoader", "()Ljava/lang/ClassLoader;")
interpreter.rs:30189  find("java/lang/ClassLoader", "setDefaultAssertionStatus", "(Z)V")
interpreter.rs:30501  find("java/lang/foreign/DowncallHandle", "type", "()Ljava/lang/invoke/MethodType;")
interpreter.rs:39421  find("java/lang/reflect/Method", "invoke", "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;")
interpreter.rs:39430  find("java/lang/reflect/Constructor", "newInstance", "([Ljava/lang/Object;)Ljava/lang/Object;")
interpreter.rs:39439  find("org/apache/maven/surefire/junitplatform/LazyLauncher", "discover", "(L…LauncherDiscoveryRequest;)L…TestPlan;")
vm_exec.rs:11371      find("java/lang/foreign/DowncallHandle", "type", "()Ljava/lang/invoke/MethodType;")
vm_exec.rs:15253      find("java/nio/file/Path", "toString", "()Ljava/lang/String;")
```

Plus `interpreter.rs:30217` — `find(LAZY, "discover", DESC_DISCOVER)`, where
both literals are `const`s declared at `:30209` / `:30211`, so it is a
fully-constant triple in everything but syntax — and `interpreter.rs:5468`,
where the class is a `&'static str` selected by a `match` on the call site's
own class name.

The rest of the bucket has a literal class and takes `method_name` /
`descriptor` from the call site: `interpreter.rs:29580, 29653, 29704, 29822,
29858, 29965, 30007, 30086, 30154, 30399, 30442, 30463, 30521, 30544, 30556,
30568, 30760, 30802, 41390, 41430`; `vm_exec.rs:11393, 11435, 11466, 11490,
15281, 15303, 15342`.

**(c) Pure call-site triple — ~35 sites.** `find(class_name, method_name,
descriptor)` where all three come from the invoking constant-pool entry.
Highest-value bucket: `interpreter.rs:5176, 5478, 5639, 29758, 29916, 30421,
30538` (**the primary `try_stackless_invoke` lookup**), `31056, 31331, 32019,
32946, 34522, 34528, 34746, 35152, 36102, 36175, 41049, 41638`;
`vm_exec.rs:5131, 11636` (`find_with_kind` — documented in-tree as "the single
native dispatch VM-wide"), `11705, 12175, 15215, 15363, 15472, 17996, 18340,
18649, 19098, 19183, 19427, 19518`.

**(d) Genuinely dynamic — ~32 sites.** The class name comes from the
*receiver's runtime class* or from a superclass/interface walk, so it is not a
function of the call site alone: `interpreter.rs:5339, 5399, 23573, 30642,
30978, 31351, 32172, 32202, 38988, 39070, 40878, 41165, 41202, 41255, 41347`;
`vm_exec.rs:11748, 11759, 15399, 18032, 18099, 18122, 18142, 18163, 18193,
18316, 18330, 18424, 18512, 18548, 18761, 18766, 18787`. A monomorphic inline
cache keyed on `(call site, receiver ClassId)` would cover most of these, but
that is a dispatch-layer change and explicitly **out of scope for this wave** —
`NativeMethodId` is the primitive such a cache would store. One further site,
`interpreter.rs:24036` (`find(iface, …)` in lambda dispatch), is per-lambda-
record rather than per-call-site.

---

## 2. What landed (this wave, `native-api` only)

### 2.1 A dense slot table replaces the callback map

`NativeMethodRegistry` previously carried two parallel digest-keyed maps:

```rust
methods:          FxHashMap<(u64,u64), NativeCallback>,
category_by_key:  FxHashMap<(u64,u64), NativeKind>,
```

Both are replaced by one indirection:

```rust
slots:        Vec<NativeSlot>,                 // NativeSlot { callback, kind, reg_index }
slot_by_key:  FxHashMap<(u64,u64), u32>,       // digest -> slot index
```

Consequences:

* **A `NativeMethodId` is just the slot index.** Redeeming one
  (`callback_of`) is a bounds-checked array load — no hash, no string walk.
* `find`, `find_with_kind` and `kind_of` all go through the *same* table, so
  there is one mechanism, not two. `find_with_kind` no longer needs a second
  map at all.
* One fewer `FxHashMap` (~3,100 entries) is built at boot.

`register()` gained no map insert on the common path (the `methods` insert
became the `slot_by_key` insert); it gained one `Vec::push`.

### 2.2 Handle stability

Re-registering an already-registered triple **updates its slot in place**
instead of appending a new one. This is what makes a handle stable: a call site
that memoized `NativeMethodId(k)` before a re-registration keeps working, and
automatically observes the new callback — matching `register`'s documented
last-registration-wins contract. `alias_class` (which re-registers by design)
and the `register_*` passes that deliberately promote a fixed stub to
`Intrinsic` both rely on this.

Ids are never reused or invalidated: the registry has no removal path.

### 2.3 Full-name verification on every digest hit (soundness)

**Every** digest lookup now funnels through one private function:

```rust
fn slot_index_for_key(&self, key: (u64,u64), class: &str, method: &str, desc: &str) -> Option<u32>
```

which fetches the triple the candidate slot was registered under (via
`NativeSlot::reg_index` into the append-only `registrations` log) and compares
all three strings. **A digest hit whose name does not match is reported as a
miss, never as a callback.**

This is required, not decorative. `classloading/src/class_manager.rs`
(`loaded_classes` field doc at line 1233, "Round 4 audit fix (CRIT)") records a
shipped defect of exactly this shape: a `name_to_id: FxHashMap<u64, ClassId>`
shadow map keyed by a raw FNV-1a digest **with no name verification**, where any
collision returned the wrong `ClassId` and caused silent type confusion
downstream. That map was deleted. Nothing added here repeats it — including the
`*_by_key` entry points, which take the precomputed digest *and* the three
strings, and verify the strings.

Cost: three `str` comparisons (SIMD memcmp over already-hot cache lines) on a
hit, against the byte-at-a-time hash that produced the key. Net effect on
`find` is noise; the correctness gain is that a true 128-bit collision now
degrades to "the loser's triple resolves to `None`" instead of "the loser's
triple silently dispatches the winner's callback".

The registration-time `debug_assert!` collision check is **kept unchanged** —
it is still the thing that tells a developer a collision happened at all.

### 2.4 Precomputed digests

`NativeMethodKey::new(class, method, descriptor)` computes the 128-bit digest
once — intended to be built at class-link time next to the method metadata —
and `resolve_id_by_key` / `find_by_key` / `find_with_kind_by_key` accept it.
The strings are still passed and still verified; the key only removes the hash.

### 2.5 `NativeCallSite`: the shared memo cell

```rust
pub struct NativeCallSite { /* one AtomicU64 */ }

impl NativeCallSite {
    pub const fn new() -> Self;
    pub fn resolve(&self, reg, class, method, desc) -> Option<NativeMethodId>;
    pub fn resolve_with_key(&self, reg, key, class, method, desc) -> Option<NativeMethodId>;
    pub fn callback(&self, reg, class, method, desc) -> Option<NativeCallback>;
    pub fn callback_with_kind(&self, reg, class, method, desc) -> Option<(NativeCallback, NativeKind)>;
    pub fn invalidate(&self);
    pub fn is_warm(&self, reg) -> bool;
}
```

One `AtomicU64`, laid out as `(generation << 32) | (slot + 1)`, with `slot + 1
== 0` meaning "resolved, and there is no native here". Warm path is: one
relaxed load, one `u32` compare, one array index. Races are benign — two
threads may both resolve and store the identical word.

**Why a generation instead of a `OnceLock`.** The existing
`native_callback_cache` is a `OnceLock<Option<NativeCallback>>` whose stated
soundness argument is "native registration is immutable after VM boot". That
holds in steady state (`register` takes `&mut self`;
`native-api/tests/registry_build_once.rs` pins the "build once, then freeze as
`&`" contract; and `NativeRealm::native_methods` is documented "immutable after
construction") but not *during* boot, and not across `alias_class` — a `None`
memoized before those run is wrong forever. `generation()` changes exactly when
a genuinely new triple is registered (a re-registration does not move it), so
keying the memo on it makes a stale **negative** self-heal for the price of a
`u32` compare. Positive entries are revalidated for free by the same compare.

**This is the pattern `dev` independently converged on.** `dev` added
`CachedBytecodeMethod::jit_probe_generation: AtomicU64` — a memo of the global
`cratonvm_jit::jit_cache_generation()` that exists precisely so an interpreter
dispatch arm stops re-hashing and re-comparing three strings against the JIT
cache on every call, and whose stated correctness argument is "every writer
bumps the global generation". `NativeCallSite` is the same design applied to
the native registry, so the two memo fields on that struct will read alike.

**The generation is also a registry identity.** `generation()` is
`registry_epoch + slots.len()`, where `registry_epoch` is drawn from a
process-global counter in `REGISTRY_EPOCH_STRIDE` (2^20) increments at
`NativeMethodRegistry::new()`. Without that band, two registries with the same
number of registrations would report the same generation — and the `static
NativeCallSite` pattern recommended in §3 Step 1 is *process*-global, so a test
binary that builds several `SharedVm`s could hand a memo taken against registry
A to registry B and redeem a slot index that means something else there. With
the band, that is a re-resolve rather than a wrong answer. Generation `0` is
never a live value, so `NativeCallSite` can use an all-zero word as its
unambiguous "never resolved" sentinel. Covered by
`distinct_registries_do_not_share_a_generation` and
`a_memo_from_one_registry_is_not_honoured_by_another`.

`NativeCallSite` implements `Clone` (copying the memo word), which is what
`CachedBytecodeMethod`'s hand-written `Clone` needs.

### 2.6 Public API: additive only

Nothing existing changed shape. `native-api` is depended on by
`native-builtins`, `native-collections`, `native-io`, `native-awt`, `jit-api`
and `vm`; all existing signatures compile unchanged.

* Unchanged: `register`, `find`, `find_with_kind`, `kind_of`,
  `find_by_method_descriptor`, `might_have_method_descriptor`, `alias_class`,
  `len`, `is_empty`, `dump_registrations`, `with_category`,
  `flush_native_ring_names`, `NativeCallback`, `NativeKind`, `NativeContext`,
  and `dev`'s `NativeHandle` / `NativeHandleScope`.
* Added on `NativeMethodRegistry`: `generation`, `resolve_id`,
  `resolve_id_by_key`, `callback_of`, `kind_of_id`, `triple_of`, `find_by_key`,
  `find_with_kind_by_key`.
* Added at crate root: `NativeMethodId`, `NativeMethodKey`, `NativeCallSite`
  (module `native_id`).
* **No new `NativeContext` methods** — that trait is 3,000+ lines of public
  surface implemented outside this crate, and nothing here needs it.
* **No new env-var gates, no feature flags, no default-off paths.** The slot
  table and the verification are unconditional.

Behavioural notes for reviewers:

* `len()` now counts distinct triples (`slots.len()`) rather than
  `methods.len()`. These were already the same number.
* `generation()` is an **opaque token**: compare it for equality, never treat
  it as a registration count (it is offset by the per-registry epoch).
* `find_with_kind`'s descriptor-quirk fallback still reports `Bridge` when the
  original (un-rewritten) descriptor has no category. The slot now carries the
  true kind and could report it exactly, but that would change which natives
  the real-JDK `SyntheticStub` drop applies to on quirky descriptors — a
  dispatch-semantics change, deliberately not made here. `find_with_kind_by_key`
  (new, no existing callers) does report the exact kind.
* `alias_class` was split into two passes so the `registrations` borrow is
  provably released before the whole-`self` slot probe. Rare boot path;
  behaviour identical.
* Merge resolution: `dev`'s only overlapping edit was a rustfmt reflow of
  `find_with_kind`'s two-map probe, which this change deletes. Its formatting
  of the surviving `kind_of` chain was carried over; the reflowed probe is
  gone. Noted inline at the site.

### 2.7 Tests added

In `native-api/src/registry.rs`:

* `handle_lookup_agrees_with_name_lookup` — for several triples,
  `find(t) == resolve_id(t).and_then(callback_of)`, and misses agree too.
* `handle_survives_later_registrations` — 256 unrelated registrations do not
  move an existing handle.
* `handle_is_stable_across_reregistration_and_picks_up_new_callback` — the
  slot is updated in place; generation does not advance; the handle now yields
  the new callback.
* `generation_advances_only_for_new_triples`,
  `distinct_registries_do_not_share_a_generation`.
* `digest_collision_is_reported_as_a_miss_not_a_wrong_callback` — a
  `#[cfg(test)]`-only injector points the digest of an unregistered triple at a
  registered slot (a real 128-bit collision is computationally infeasible to
  find, which is exactly why the check must be tested by injection). Asserts
  `find` / `resolve_id` / `kind_of` / `find_with_kind` / `find_by_key` all
  return `None` for the victim, and that the legitimate owner is unaffected.
* `precomputed_key_matches_name_lookup` — including that a key which does not
  describe the strings simply misses.
* `handles_resolve_through_the_descriptor_quirk_path` — a memoized handle must
  not lose `find`'s compatibility rewrites.
* `kind_travels_with_the_handle`, `foreign_handle_does_not_panic`.

In `native-api/src/native_id.rs`:

* `warm_call_site_returns_the_same_callback`,
  `memoized_negative_self_heals_when_a_native_is_registered_later`,
  `re_registration_is_seen_through_a_warm_call_site`,
  `call_site_agrees_with_direct_lookup_including_kind`,
  `resolve_with_key_matches_resolve`, `invalidate_forces_a_re_resolve`,
  `clone_preserves_the_memo`,
  `a_memo_from_one_registry_is_not_honoured_by_another`,
  `id_round_trips_through_a_raw_u32`.

Callback identity is compared as `cb as usize` (the workspace already does this
in `register`'s native-ring index; `fn`-pointer `==` trips
`unpredictable_function_pointer_comparisons` and is unreliable under function
merging). The two test callbacks have observably different bodies, so they
cannot be merged.

---

## 3. Consumer spec for the next wave (dev @ 6495a191c)

**Files the next agent must edit (not owned by this wave):**
`jit-api/src/lib.rs`, `vm/src/runtime/interpreter.rs`, `vm/src/vm/vm_exec.rs`.
Everything below is a drop-in: `NativeCallSite::callback(reg, c, m, d)` returns
exactly what `reg.find(c, m, d)` returns.

### Step 0 — add the cache field (`jit-api/src/lib.rs`)

`CachedBytecodeMethod` (declared at `jit-api/src/lib.rs:40`) already carries
five memo fields: `force_native_cache: OnceLock<bool>` (`:70`),
`native_callback_cache: OnceLock<Option<NativeCallback>>` (`:86`),
`invoc_key: OnceLock<u64>` (`:100`), `jit_probe_generation: AtomicU64` (`:125`)
and `quickened: OnceLock<...>` (`:144`). Add:

```rust
/// Shared native-dispatch memo for this call site. Supersedes
/// `native_callback_cache`: same steady-state cost, but keyed on the
/// registry generation so a memoized "no native" cannot go stale — the
/// same argument `jit_probe_generation` above already relies on.
pub native_call_site: cratonvm_native_api::NativeCallSite,
```

Two things changed on `dev` that make this easier than the original plan:

* The struct now has a **hand-written `Clone`** at `jit-api/src/lib.rs:147`
  (because `jit_probe_generation` is an `AtomicU64`). Add one line to it:
  `native_call_site: self.native_call_site.clone(),`. `NativeCallSite: Clone`
  is already provided, and the field doc there already argues that copying a
  memo forward is sound.
* `jit_probe_generation` is precedent: a generation-keyed atomic memo on this
  exact struct, with the same rationale. Follow its doc style.

`CachedBytecodeMethod` is constructed in **32** places; all use struct literals
containing `native_callback_cache: std::sync::OnceLock::new()`, so add
`native_call_site: cratonvm_native_api::NativeCallSite::new()` beside each:

```
classloading/src/resolution.rs        3
jit/src/lib.rs                       12
jit/tests/ir_vs_singlepass.rs         1
jit-api/src/lib.rs                    1
vm/src/jit/helpers.rs                 1
vm/src/runtime/interpreter.rs        10
vm/src/runtime/lockfree_resolve.rs    1
vm/src/runtime/vtable.rs              2
vm/src/vm.rs                          1
```

(`grep -rn 'native_callback_cache: std::sync::OnceLock::new()'` enumerates them
exactly; they move on every `dev` merge, so derive rather than copy a list.)

Then retire `native_callback_cache` in the same pass (see A1-A3 below) so there
is one mechanism, not two.

### Step 1 — highest value, zero risk: fully-constant triples

Each becomes a file-local `static` cell. No struct changes, no threading of
parameters. Pattern (note the `natives` hop, new on `dev`):

```rust
// before
let cb = shared.natives.native_methods.find(
    "java/lang/Class", "getClassLoader", "()Ljava/lang/ClassLoader;")?;

// after
static NCS_CLASS_GET_CLASSLOADER: cratonvm_native_api::NativeCallSite =
    cratonvm_native_api::NativeCallSite::new();
let cb = NCS_CLASS_GET_CLASSLOADER.callback(
    &shared.natives.native_methods,
    "java/lang/Class", "getClassLoader", "()Ljava/lang/ClassLoader;")?;
```

(`NativeCallSite::new()` is `const fn`, so a `static` works.)

| # | File:line | Current call | Replacement |
|---|---|---|---|
| B1 | `interpreter.rs:29618` | `find("java/lang/Class","getClassLoader","()Ljava/lang/ClassLoader;")` | static cell `NCS_CLASS_GET_CLASSLOADER` |
| B2 | `interpreter.rs:30189` | `find("java/lang/ClassLoader","setDefaultAssertionStatus","(Z)V")` | static cell |
| B3 | `interpreter.rs:30217` | `find(LAZY,"discover",DESC_DISCOVER)` (consts at `:30209`/`:30211`) | static cell |
| B4 | `interpreter.rs:30501` | `find("java/lang/foreign/DowncallHandle","type","()Ljava/lang/invoke/MethodType;")` | static cell (share with B8) |
| B5 | `interpreter.rs:39421` | `find("java/lang/reflect/Method","invoke","(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;")` | static cell |
| B6 | `interpreter.rs:39430` | `find("java/lang/reflect/Constructor","newInstance","([Ljava/lang/Object;)Ljava/lang/Object;")` | static cell |
| B7 | `interpreter.rs:39439` | `find("org/apache/maven/surefire/junitplatform/LazyLauncher","discover",…)` | static cell (same triple as B3 — share one) |
| B8 | `vm_exec.rs:11371` | `find("java/lang/foreign/DowncallHandle","type","()Ljava/lang/invoke/MethodType;")` | static cell |
| B9 | `vm_exec.rs:15253` | `find("java/nio/file/Path","toString","()Ljava/lang/String;")` | static cell |

B5-B7 sit in `native_override_for_cached_reflect_invoke`
(`interpreter.rs:39410`), a three-arm `match` on `(class, method, descriptor)`
where every arm is a constant triple — the cleanest single conversion in the
file.

Near-constant (class + descriptor literal, `method_name` from a 2-3 element
`matches!` set): `interpreter.rs:30521, 30760, 30802`; `vm_exec.rs:11393` — all
`find("…/DowncallHandle" | ".../MethodHandle", method_name,
"([Ljava/lang/Object;)Ljava/lang/Object;")`. Use a small `static` array of
three cells indexed by `invoke`/`invokeExact`/`invokeBasic`, or leave them
(they sit behind a `DowncallHandle` receiver-class check that already costs a
`class_manager.read()` — the hash is not the bottleneck there).

### Step 2 — the per-invocation lookups the profiles pointed at

| # | File:line | Function (decl) | Current call | Replacement |
|---|---|---|---|---|
| A1 | `interpreter.rs:29913` | `intercept_force_registered_native_cached` (`:29794`) | `cached.native_callback_cache.get_or_init(\|\| …find(class_name, method_name, method_descriptor))` | `cached.native_call_site.callback(&shared.natives.native_methods, class_name, method_name, method_descriptor)` |
| A2 | `interpreter.rs:39215` | vtable-hit force-native gate | `cached.native_callback_cache.get_or_init(…).is_some()` | `cached.native_call_site.resolve(&shared.natives.native_methods, cached.class_name.as_ref(), cached.method_name.as_ref(), cached.method_descriptor.as_ref()).is_some()` |
| A3 | `interpreter.rs:40054` | `execute_invokevirtual_cached` (`:39451`) | `cached.native_callback_cache.get_or_init(…).is_some()` | same shape as A2 |
| A4 | `vm_exec.rs:11636` | `invoke_or_native` (`:11300`) | `find_with_kind(effective_class, method_name, descriptor)` | needs a call-site cell — see below |
| A5 | `interpreter.rs:30538` | `try_stackless_invoke` (`:30339`) | `find(class_name, method_name, descriptor)` | needs a call-site cell — see below |

A1-A3 are mechanical (all three already share one `native_callback_cache`, so
they convert together) and also **fix a latent bug**: `native_callback_cache`
memoizes `None` permanently, so a native registered by a lazy registrar after
this call site first executes is never seen. `NativeCallSite` re-resolves when
the generation moves. Delete `native_callback_cache` from `jit-api/src/lib.rs`
(field at `:86`, `Clone` line at `:163`) once all three are converted.

A4/A5 are the highest-value sites but neither function receives a
`CachedBytecodeMethod` — both take bare `&str`s:

```rust
fn try_stackless_invoke(shared, thread, frame_idx, class_name: &str,
                        method_name: &str, descriptor: &str, args, …)   // :30339
pub fn invoke_or_native(shared, thread, class_name: &str,
                        method_name: &str, descriptor: &str, args, …)   // :11300
```

Recommended shape: add a trailing
`native_site: Option<&cratonvm_native_api::NativeCallSite>` parameter and pass
`Some(&cached.native_call_site)` from callers that have one, `None` elsewhere:

```rust
let native_cb = match native_site {
    Some(site) => site.callback(&shared.natives.native_methods, class_name, method_name, descriptor),
    None => shared.natives.native_methods.find(class_name, method_name, descriptor),
};
```

Callers of `try_stackless_invoke`: `interpreter.rs:22266` (invokevirtual /
invokespecial) and `:31682` (invokestatic) — both have a cached entry in scope.
Callers of `intercept_force_registered_native` (the uncached sibling, declared
`:29548`) are `interpreter.rs:22003, 30409, 31584, 39320`; the one at `:39320`
has an `entry_cached: &CachedBytecodeMethod` in scope and could simply switch
to `intercept_force_registered_native_cached` instead. For A4 use
`callback_with_kind`, which returns `(NativeCallback, NativeKind)` from the
same single resolve, exactly matching `find_with_kind`.

**Caution for A5:** `try_stackless_invoke` rewrites the effective class before
the lookup (`if class_name.starts_with('[') { "java/lang/Object" }`), and the
lookup at `:30538` sits inside an `.or_else` chain whose earlier arms
(`:30501`, `:30521`, and the `surefire_lazy_launcher_discover_native` probe)
can already have produced a callback. Memoize only the `:30538` arm, keyed on
the *post-rewrite* `class_name`. Do not hoist the memo above the
receiver-dependent arms — they are not call-site-pure.

### Step 3 — constant-class / call-site-triple sites

Mechanical once Step 0 lands; each site takes its own `NativeCallSite`. A
single `CachedBytecodeMethod` cannot back more than one *distinct* triple, so
sites that look up a different triple than the call site's own (e.g.
`find("java/lang/ClassLoader", method_name, method_descriptor)` inside a
`ClassLoader`-owner interception) need either their own field or a small
`[NativeCallSite; N]` on the cached entry. Recommended: land Steps 0-2 first,
measure, and only extend if the profile still shows `native_method_hash`.

Line lists are in §1.2 buckets (b) and (c).

### Step 4 — out of scope, recorded for later

The ~32 receiver-class-dependent sites in §1.2(d) want a monomorphic inline
cache keyed on `(call site, receiver ClassId)`. `NativeMethodId` is the value
such a cache would store; the cache itself is a dispatch-layer change and
belongs in a later wave. Also unconverted: the superclass-walk loops
(`interpreter.rs:5339`/`:5399`, `vm_exec.rs:18193`, `:18548`, `:18761`-`:18787`),
which call `find` once per level of the hierarchy.

### Verification checklist for the next wave

1. `NativeCallSite::callback(reg, c, m, d)` must be substitutable for
   `reg.find(c, m, d)` at every converted site — including on a **miss**
   (both return `None`) and on a **quirky descriptor** (both apply the
   compatibility rewrite; covered by
   `handles_resolve_through_the_descriptor_quirk_path`).
2. A converted site must not be reached with a *different* triple than the one
   it first memoized. The generation check does not protect against this — the
   memo is per call site, not per triple. Every site in Steps 1-3 is
   call-site-pure by construction; re-verify if a site is moved.
3. Re-derive every line number in this section before editing. `dev` moves
   fast: `interpreter.rs` gained 3,919 lines in the three days between
   `origin/main@e4e4053bb` and `dev@6495a191c`, and `SharedVm.native_methods`
   became `SharedVm.natives.native_methods` in that window.
4. `cb as usize` before/after a conversion is the cheapest A/B check:
   `CRATONVM_ENABLE_NATIVE_RING=1` plus `flush_native_ring_names()` resolves a
   callback pointer back to `class.method desc`.
