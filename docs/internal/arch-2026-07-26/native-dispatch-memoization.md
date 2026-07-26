# Native-dispatch memoization

Status: **landed in `native-api` (2026-07-26). Consumer adoption in
`vm/src/runtime/interpreter.rs` + `vm/src/vm/vm_exec.rs` is the next wave.**

Slug: `native-dispatch-memoization`. Owner crate: `native-api` only.

---

## 1. The finding, verified

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
| `perf` on `TestResponsePerformance` | `NativeMethodRegistry::find` at ~7% of samples — #2 hottest symbol behind the interpreter's frame-dispatch loop | `CachedBytecodeMethod::native_callback_cache` (`jit-api/src/lib.rs:81`), a `OnceLock<Option<NativeCallback>>` |
| `gdb` sampling of a hung-looking H2 `TestFileSystem.testConcurrent` | both live threads repeatedly inside `hash_byte_pair` / `native_method_hash` | `NativeMethodRegistry::find_with_kind`, folding a redundant second `kind_of` hash into the first probe |

### 1.1 Census of `find` call sites

Measured by a multiline scan for `native_methods.{find,find_with_kind,
find_by_method_descriptor}(` (the naive line-oriented grep **undercounts by
~4x**, because most call sites are rustfmt-wrapped with `.native_methods` and
`.find(` on separate lines — the primary `try_stackless_invoke` lookup at
`interpreter.rs:29655` is one of the sites a line grep misses). Sites inside
`#[cfg(test)]` modules are excluded from the production count.

| File | Production sites | Test-only sites |
|---|---|---|
| `vm/src/runtime/interpreter.rs` | **63** | 0 |
| `vm/src/vm/vm_exec.rs` | **40** | 0 |
| `vm/src/jit/helpers.rs` | 2 | 0 |
| `vm/src/vm/vm_util.rs` | 1 | 0 |
| `vm/src/vm/vm_object.rs` | 1 | 0 |
| `vm/src/vm.rs` | 0 | 35 |
| `vm/src/vm/vm_init.rs` | 0 | 33 (all past the `mod tests` at line 6128) |
| `vm/src/runtime/instrument.rs` | 0 | 7 |
| `native-builtins/*` | 0 | 7 |
| **Total production** | **107** | — |

**Memoized today: 1 mechanism, covering 2 sites.**
`CachedBytecodeMethod::native_callback_cache` is populated at
`interpreter.rs:29064` (inside `intercept_force_registered_native_cached`) and
re-read at `interpreter.rs:38857` (the JIT virtual-tier-up
`has_registered_native` guard). Nothing else caches anything.

So ARCHITECTURE.md's "several interpreter sites still re-resolve per
invocation" is **true but understates the situation by an order of magnitude**:
**62 of 63** production interpreter sites re-resolve on every invocation, as do
**40 of 40** in `vm_exec.rs`. The correct statement is "one interpreter site is
memoized; every other native lookup in the VM re-hashes three strings per
call".

### 1.2 Classification

Classifying by *what determines the triple being looked up*:

**(a) Already memoized — 2 sites.** `interpreter.rs:29064`, `:38857`.

**(b) Fully-constant triples — 8 sites.** All three arguments are string
literals, so the lookup is a compile-time constant that is nonetheless
recomputed on every call:

```
interpreter.rs:28771  find("java/lang/Class", "getClassLoader", "()Ljava/lang/ClassLoader;")
interpreter.rs:29334  find("java/lang/ClassLoader", "setDefaultAssertionStatus", "(Z)V")
interpreter.rs:29620  find("java/lang/foreign/DowncallHandle", "type", "()Ljava/lang/invoke/MethodType;")
interpreter.rs:38314  find("java/lang/reflect/Method", "invoke", "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;")
interpreter.rs:38323  find("java/lang/reflect/Constructor", "newInstance", "([Ljava/lang/Object;)Ljava/lang/Object;")
interpreter.rs:38332  find("org/apache/maven/surefire/junitplatform/LazyLauncher", "discover", "(L…LauncherDiscoveryRequest;)L…TestPlan;")
vm_exec.rs:10725      find("java/lang/foreign/DowncallHandle", "type", "()Ljava/lang/invoke/MethodType;")
vm_exec.rs:14455      find("java/nio/file/Path", "toString", "()Ljava/lang/String;")
```

Plus near-constant variants where the class and descriptor are literals and
only `method_name` varies over a 2-3 element enumerated set
(`interpreter.rs:29639`, `:29854`, `:29896`; `vm_exec.rs:10746`), and
`interpreter.rs:29358` where the literals live in `const LAZY` / `const
DESC_DISCOVER`.

**(c) Constant class, call-site method/descriptor — ~25 sites.** e.g.
`find("java/lang/ClassLoader", method_name, method_descriptor)`
(`interpreter.rs:28736`, `:28975`, `:29300`; `vm_exec.rs:10787`, `:10817`,
`:10840`), `find("java/lang/Class", …)` (`:28808`, `:29010`),
`find("java/net/HttpURLConnection", …)` (`:28858`),
`find("java/util/zip/ZipFile", …)` (`:29520`),
`find("java/lang/reflect/Method"/"Constructor", …)` (`:29562`, `:29585`),
`find("javax/net/ssl/SSLContext"/"SSLSocketFactory"/"SSLSocket", …)`
(`interpreter.rs:29661`, `:29673`, `:29685`; `vm_exec.rs:14483`, `:14507`,
`:14543`). Memoizable per call site.

**(d) Pure call-site triple — ~30 sites.** `find(class_name, method_name,
descriptor)` where all three come from the invoking constant-pool entry:
`interpreter.rs:28909`, `:29655` (**the primary `try_stackless_invoke`
lookup**), `:31007`, `:35023`, `:5048`, `:5498`; `vm_exec.rs:4799`,
`:10978` (`find_with_kind` — documented in-tree as "the single native dispatch
VM-wide"), `:11046`, `:11473`, `:14417`, `:14562`, `:14665`, `:17189`,
`:18352`. Memoizable per call site — this is the highest-value bucket.

**(e) Genuinely dynamic — ~40 sites.** The class name comes from the
*receiver's runtime class* or from a superclass/interface walk, so it is not a
function of the call site alone: `find(&cls.name, …)` /
`find(&parent.name, …)` / `find(recv_name, …)` — `interpreter.rs:5207`,
`:5266`, `:22769`, `:29737`, `:37901`, `:37982`, `:39656`;
`vm_exec.rs:11091`, `:11102`, `:17224`, `:17290`, `:17312`, `:17331`,
`:17351`, `:17380`, `:17695`, `:17940`, `:17947`, `:17968`. A monomorphic
inline cache keyed on `(call site, receiver ClassId)` would cover most of
these, but that is a dispatch-layer change and explicitly **out of scope for
this wave** — `NativeMethodId` is the primitive such a cache would store.

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
(`loaded_classes` field doc, "Round 4 audit fix (CRIT)") records a shipped
defect of exactly this shape: a `name_to_id: FxHashMap<u64, ClassId>` shadow
map keyed by a raw FNV-1a digest **with no name verification**, where any
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
holds in steady state (`register` takes `&mut self`, and
`native-api/tests/registry_build_once.rs` pins the "build once, then freeze as
`&`" contract) but not *during* boot, and not across `alias_class` or a lazy
`register_*` pass — a `None` memoized before those run is wrong forever.
`generation()` changes exactly when a genuinely new triple is registered (a
re-registration does not move it), so keying the memo on it makes a stale
**negative** self-heal for the price of a `u32` compare. Positive entries are
revalidated for free by the same compare.

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

`NativeCallSite` implements `Clone` (copying the memo word) because
`CachedBytecodeMethod` derives `Clone`.

### 2.6 Public API: additive only

Nothing existing changed shape. `native-api` is depended on by
`native-builtins`, `native-collections`, `native-io`, `native-awt`, `jit-api`
and `vm`; all existing signatures compile unchanged.

* Unchanged: `register`, `find`, `find_with_kind`, `kind_of`,
  `find_by_method_descriptor`, `might_have_method_descriptor`, `alias_class`,
  `len`, `is_empty`, `dump_registrations`, `with_category`,
  `flush_native_ring_names`, `NativeCallback`, `NativeKind`, `NativeContext`.
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

## 3. Consumer spec for the next wave

**Files the next agent must edit (not owned by this wave):**
`jit-api/src/lib.rs`, `vm/src/runtime/interpreter.rs`, `vm/src/vm/vm_exec.rs`.
Everything below is a drop-in: `NativeCallSite::callback(reg, c, m, d)` returns
exactly what `reg.find(c, m, d)` returns.

### Step 0 — add the cache field (`jit-api/src/lib.rs`)

`CachedBytecodeMethod` (line ~34, `#[derive(Clone)]`) already carries
`force_native_cache: OnceLock<bool>` and
`native_callback_cache: OnceLock<Option<NativeCallback>>`. Add:

```rust
/// Shared native-dispatch memo for this call site. Supersedes
/// `native_callback_cache`: same steady-state cost, but keyed on the
/// registry generation so a memoized "no native" cannot go stale.
pub native_call_site: cratonvm_native_api::NativeCallSite,
```

`CachedBytecodeMethod` is constructed in 29 places; all of them use struct
literals with `native_callback_cache: std::sync::OnceLock::new()`, so add
`native_call_site: cratonvm_native_api::NativeCallSite::new()` beside each:

```
classloading/src/resolution.rs:1631, 1701, 1736
jit/src/lib.rs:8805, 9025, 9100, 9559, 9694, 9807, 9924, 9983, 10045, 11326, 11342
jit/tests/ir_vs_singlepass.rs:145
jit-api/src/lib.rs:677
vm/src/jit/helpers.rs:1732
vm/src/runtime/interpreter.rs:12252, 12273, 12769, 22795, 31220, 34234, 35122, 40280, 44466
vm/src/runtime/lockfree_resolve.rs:987
vm/src/runtime/vtable.rs:822, 1531
```

(Alternatively `#[derive(Default)]`-style: give the field a `Default` and use
`..Default::default()` — but the struct has no `Default` today, so the explicit
initializer is the smaller change.)

Then retire `native_callback_cache` in the same pass (see A1/A2 below) so there
is one mechanism, not three.

### Step 1 — highest value, zero risk: fully-constant triples

Each becomes a file-local `static` cell. No struct changes, no threading of
parameters. Pattern:

```rust
// before
let cb = shared.native_methods.find("java/lang/Class", "getClassLoader",
                                    "()Ljava/lang/ClassLoader;")?;
// after
static NCS_CLASS_GET_CLASSLOADER: cratonvm_native_api::NativeCallSite =
    cratonvm_native_api::NativeCallSite::new();
let cb = NCS_CLASS_GET_CLASSLOADER.callback(
    &shared.native_methods, "java/lang/Class", "getClassLoader",
    "()Ljava/lang/ClassLoader;")?;
```

(`NativeCallSite::new()` is `const fn`, so a `static` works.)

| # | File:line | Current call | Replacement |
|---|---|---|---|
| B1 | `interpreter.rs:28771` | `find("java/lang/Class","getClassLoader","()Ljava/lang/ClassLoader;")` | static cell `NCS_CLASS_GET_CLASSLOADER` |
| B2 | `interpreter.rs:29334` | `find("java/lang/ClassLoader","setDefaultAssertionStatus","(Z)V")` | static cell |
| B3 | `interpreter.rs:29358` | `find(LAZY,"discover",DESC_DISCOVER)` (consts) | static cell |
| B4 | `interpreter.rs:29620` | `find("java/lang/foreign/DowncallHandle","type","()Ljava/lang/invoke/MethodType;")` | static cell (share with B7) |
| B5 | `interpreter.rs:38314` | `find("java/lang/reflect/Method","invoke","(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;")` | static cell |
| B6 | `interpreter.rs:38323` | `find("java/lang/reflect/Constructor","newInstance","([Ljava/lang/Object;)Ljava/lang/Object;")` | static cell |
| B7 | `interpreter.rs:38332` | `find("org/apache/maven/surefire/junitplatform/LazyLauncher","discover",…)` | static cell |
| B8 | `vm_exec.rs:10725` | `find("java/lang/foreign/DowncallHandle","type","()Ljava/lang/invoke/MethodType;")` | static cell |
| B9 | `vm_exec.rs:14455` | `find("java/nio/file/Path","toString","()Ljava/lang/String;")` | static cell |

Near-constant (class + descriptor literal, `method_name` from a 2-3 element
`matches!` set): `interpreter.rs:29639`, `:29854`, `:29896`; `vm_exec.rs:10746`
— all `find("…/DowncallHandle" or ".../MethodHandle", method_name,
"([Ljava/lang/Object;)Ljava/lang/Object;")`. Use a small `static` array of
three cells indexed by `invoke`/`invokeExact`/`invokeBasic`, or leave them
(they are behind a `DowncallHandle` receiver-class check that already costs a
`class_manager.read()` — the hash is not the bottleneck there).

### Step 2 — the two hot per-invocation lookups

These are the ones the profiles actually pointed at.

| # | File:line | Function | Current call | Replacement |
|---|---|---|---|---|
| A1 | `interpreter.rs:29064` | `intercept_force_registered_native_cached` | `cached.native_callback_cache.get_or_init(\|\| shared.native_methods.find(class_name, method_name, method_descriptor))` | `cached.native_call_site.callback(&shared.native_methods, class_name, method_name, method_descriptor)` |
| A2 | `interpreter.rs:38857` | `execute_invokevirtual_cached` | `cached.native_callback_cache.get_or_init(…).is_some()` | `cached.native_call_site.resolve(&shared.native_methods, &cached.class_name, &cached.method_name, &cached.method_descriptor).is_some()` |
| A3 | `vm_exec.rs:10978` | `invoke_or_native` | `shared.native_methods.find_with_kind(effective_class, method_name, descriptor)` | needs a call-site cell — see note below |
| A4 | `interpreter.rs:29655` | `try_stackless_invoke` | `shared.native_methods.find(class_name, method_name, descriptor)` | needs a call-site cell — see note below |

A1/A2 are mechanical and also **fix a latent bug**: `native_callback_cache`
memoizes `None` permanently, so a native registered by a lazy `register_*` pass
after this call site first executes is never seen. `NativeCallSite` re-resolves
when the generation moves. Delete `native_callback_cache` from
`jit-api/src/lib.rs` once both are converted.

A3/A4 are the highest-value sites but neither function receives a
`CachedBytecodeMethod` — both take bare `&str`s:

```rust
fn try_stackless_invoke(shared, thread, frame_idx, class_name: &str,
                        method_name: &str, descriptor: &str, args, …)
fn invoke_or_native(shared, thread, class_name: &str,
                    method_name: &str, descriptor: &str, args, …)
```

Recommended shape: add a trailing
`native_site: Option<&cratonvm_native_api::NativeCallSite>` parameter and pass
`Some(&cached.native_call_site)` from callers that have one, `None` elsewhere:

```rust
let native_cb = match native_site {
    Some(site) => site.callback(&shared.native_methods, class_name, method_name, descriptor),
    None => shared.native_methods.find(class_name, method_name, descriptor),
};
```

Callers of `try_stackless_invoke` that have a cached entry in scope:
`interpreter.rs:21519` (invokevirtual/invokespecial) and `:30699`
(invokestatic). Callers of `invoke_or_native` should be audited the same way.
For A3 use `callback_with_kind`, which returns `(NativeCallback, NativeKind)`
from the same single resolve, exactly matching `find_with_kind`.

**Caution for A4:** `try_stackless_invoke`'s effective class is rewritten
before the lookup (`if class_name.starts_with('[') { "java/lang/Object" }`),
and the lookup at `:29655` sits inside an `.or_else` chain whose earlier arms
(`:29620`, `:29639`, the `surefire_lazy_launcher_discover_native` probe) can
already have produced a callback. Memoize only the `:29655` arm, keyed on the
*post-rewrite* `class_name`. Do not hoist the memo above the receiver-dependent
arms — they are not call-site-pure.

### Step 3 — constant-class / call-site-triple sites

Mechanical once Step 0 lands; each site takes its own `NativeCallSite`. A
single `CachedBytecodeMethod` cannot back more than one *distinct* triple, so
sites that look up a different triple than the call site's own (e.g.
`find("java/lang/ClassLoader", method_name, method_descriptor)` inside a
`ClassLoader`-owner interception) need either their own field or a small
`[NativeCallSite; N]` on the cached entry. Recommended: land Steps 0-2 first,
measure, and only extend if the profile still shows `native_method_hash`.

Sites, for reference:
`interpreter.rs:28736, 28808, 28858, 28909, 28975, 29010, 29115, 29156, 29234,
29300, 29520, 29562, 29585, 29661, 29673, 29685, 30062, 30138, 31007, 31884,
33454, 33459, 35023, 35095`;
`vm_exec.rs:4799, 10787, 10817, 10840, 11046, 11473, 14417, 14483, 14507,
14543, 14562, 14665, 17189, 18352`.

### Step 4 — out of scope, recorded for later

The ~40 receiver-class-dependent sites (`find(&cls.name, …)`,
`find(&parent.name, …)`, `find(recv_name, …)`) want a monomorphic inline cache
keyed on `(call site, receiver ClassId)`. `NativeMethodId` is the value such a
cache would store; the cache itself is a dispatch-layer change and belongs in a
later wave. Also unconverted: the superclass-walk loops
(`interpreter.rs:5207`/`:5266`, `vm_exec.rs:17380`, `:17940`-`:17968`), which
call `find` once per level of the hierarchy.

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
3. `cb as usize` before/after a conversion is the cheapest A/B check:
   `CRATONVM_ENABLE_NATIVE_RING=1` plus `flush_native_ring_names()` resolves a
   callback pointer back to `class.method desc`.
