# classloading: verification routing, resolution caching, classpath scans, access control

Session slug: `classloading-verify-and-resolve`. Base: `dev = 6495a191c` (merged
into the worktree branch before any analysis; the worktree had been branched
from `origin/main = e4e4053bb`, ~114 files behind, and every conclusion below is
derived against the merged tree).

Owned files: `classloading/src/{verifier,resolution,class_path,access_control}.rs`.
Everything else cited is read-only for this session; requested edits in files
owned by others are collected in [§5](#5-cross-owner-requests).

**Not built or run.** Nine agents share the host and builds are prohibited this
session; the orchestrator builds after merging. Changed files were parser- and
format-checked with `rustfmt --check` only.

---

## 0. Dependency on the sibling `type_maps` branch

`classloading/src/type_maps.rs` (branch `worktree-agent-a1873d460b7ce608c`, doc
`docs/internal/arch-2026-07-26/verifier-type-maps.md`) **is not yet on `dev`**.
The verifier work in §1 uses its `MethodTypeMapsBuilder` / `ClassTypeMaps` /
`publish_class_type_maps` API and will not compile without it. Merge order:
`type_maps` branch first, then this one. The coupling is four `use` items plus
the plumbing in `verifier.rs`; if that branch is dropped, this file's §1 changes
must be reverted with it.

API used, verified against that branch's source (not its prose):

```rust
MethodTypeMapsBuilder::new(max_locals: u16, max_stack: u16)
    .reserve(insn_hint: usize)
    .record(pc: u32, frame: &VerificationFrame)
    .observe_instruction(insn: &Instruction)
    .mark_unsafe_for_fast_path(FastPathVeto)
    .finish(walk_complete: bool) -> MethodTypeMaps
ClassTypeMaps::new(Vec<(Arc<str>, Arc<str>, Option<MethodTypeMaps>)>)
type_maps::publish_class_type_maps(ClassId, ClassTypeMaps) -> bool  // CAS, first writer wins
```

---

## 1. The `jsr`/`ret` verification bypass

### 1.1 What the bypass actually was

The sibling reported it as "`jsr`/`ret` classes get no maps at all". That
undersells it. `verify_class_bytecode_inner` (`classloading/src/verifier.rs`)
makes a **class-level** routing decision:

```
any method in the class uses jsr/jsr_w/ret ?
  no  -> bytecode_verifier::verify_bytecode(class, hierarchy)   // publishes maps
  yes -> per-method loop:
           jsr method     -> verify_method_structural_only + accept/reject
           non-jsr method -> verify_method_typestate           // published nothing
```

`verify_method_typestate` and `verify_pre_java7_inference` are full
re-implementations of `bytecode_verifier`'s two algorithms living in
`verifier.rs`. They type-state-verify their methods completely — they simply are
not the functions the sibling instrumented. So **one legacy `try-finally` in one
method cost the entire class its type maps**, including every ordinary method in
it. `verification_status` answered `Unknown` for the whole class.

That part is now fixed: both functions build the same
`MethodTypeMapsBuilder`, and the class publishes a `ClassTypeMaps` once every
method has verified. The loss is bounded to the subroutine methods themselves.

### 1.2 The decision on subroutine methods: leave conservative, explicitly

**Decision: subroutine-using methods stay unproven.** They publish a *row-less*
`MethodTypeMaps` tagged `FastPathVeto::Subroutine` (`subroutine_unproven_maps`),
so `oop_map_at` answers `None` at every pc (the contract's "unproven, scan
conservatively") and `fast_path_veto()` names the reason. This is a recorded
decision, not silence: a consumer can now distinguish it from a class that was
never verified.

**This is not "we ran out of time".** Running the existing worklist
(`verify_pre_java7_inference`) over these methods and recording its result would
produce maps that are wrong in *both* directions, which is strictly worse than
no maps:

* **Missed roots — the dominant argument.** JVMS §4.10.2.5 mandates per-call-site
  subroutine inlining precisely because a subroutine typically does not touch
  most of the caller's locals. A naive worklist merges every `jsr` call site at
  the subroutine entry, so a local holding a reference on the path from site A
  and an int on the path from site B becomes `ref ⊔ int = Top` for the whole
  subroutine body — recorded as *not a reference*.

  Elsewhere in the type system, `Top` is safe to record as "not an oop" because
  verification proves the slot is unusable and therefore dead: not scanning it
  can only fail to retain garbage. **That argument does not hold across a `ret`
  edge.** After `ret` control returns to site A, where the slot is read as a
  reference again — so it is live throughout the subroutine. Clearing its bit is
  a missed root, i.e. heap corruption. This is exactly why HotSpot does
  subroutine-aware analysis in `GenerateOopMap` rather than reusing its type
  checker's answer.

* **Non-references scanned as references.** The worklist does not even reach a
  fixpoint on these methods — the `astore` that saves the return address fails
  on the collapsed `Top` and the walk aborts (this is the documented reason the
  bypass exists at all). Rows written before the abort come from an unconverged
  state, so a slot can be recorded as a reference on the strength of one
  predecessor while an unprocessed predecessor supplies an int. Scanning an int
  as an oop is an immediate crash, not a conservative over-approximation. Note
  the asymmetry: a *converged* join can only lose references (`ref ⊔ int = Top`),
  never invent them; an *aborted* one can do either.

**Blast radius of leaving them conservative.** JVMS §4.9.1 forbids
`jsr`/`jsr_w`/`ret` at class-file major ≥ 51, and `verifier.rs` already rejects
them there by default. So the conservative set is exactly: methods in classes
compiled for Java 6 or earlier (2011 and older) that still emit old-style
`finally` subroutines. In this repo's own corpus that is ByteBuddy's
`--release 5` artifacts (`TypePool$AbstractBase$Hierarchical.clear`,
`JavaDispatcher$DynamicClassLoader.proxy`) — a handful of methods, all cold, and
now only those methods rather than their whole classes. The cost is conservative
stack scanning on those frames, which is a performance gap, not a correctness
one.

**What closing it properly needs** (not attempted here): the real §4.10.2.5
subroutine-inlining analysis — per-call-site subroutine bodies with an
"accessed locals" mask so untouched caller locals propagate from the caller
frame instead of being joined. HotSpot spends ~1k LOC on it. It is the same work
the existing load-policy comment already defers, and it must land as one piece;
a partial version that produces rows for *some* subroutine pcs would reintroduce
exactly the missed-root hazard above.

### 1.3 A soundness hazard found in the recording itself

While plumbing the builder into `verify_method_typestate` I found that
"record the frame at each instruction start" is **not** unconditionally safe in
the linear StackMapTable walk, and the guard is missing in the sibling's
`bytecode_verifier::verify_method` too. See [§5.1](#51-bytecode_verifierrs--dont-record-a-stale-frame)
— this is the highest-priority cross-owner item in this document.

`verifier.rs`'s copy now tracks an explicit `authoritative` flag alongside
`verified`, and records nothing (denying `safe_for_fast_path`) at the two pcs
where `current` is a stale frame:

1. **Dead code in a pre-Java-7 class that ships a `StackMapTable`.** The
   unreachable-code guard is conditioned on `requires_stack_map`
   (`version.major >= 51`), so for major ≤ 50 it never fires and the walk
   continues into post-`goto`/`return`/`athrow` code carrying the previous
   instruction's frame. Such a pc can still be a branch target at runtime.
2. **A handler pc that is also reachable by fall-through.** The handler frame is
   installed only under `if !verified`. When control *does* fall through,
   `current` describes the fall-through edge only; the exception edge can enter
   the same pc with different locals and `[throwable]` on the stack, and the two
   are never merged.

Type-*checking* against a stale frame is a pre-existing laxity. *Recording* one
is a different thing: it manufactures a row that the GC will trust.

`verify_pre_java7_inference` needs no such guard — it serializes a settled
worklist fixpoint whose `frame_at` includes merged exception-handler entries, so
every pc it covers is authoritative by construction.

### 1.4 Tests added (`classloading/src/verifier.rs`)

| test | pins |
|---|---|
| `subroutine_bearing_class_publishes_maps_for_its_non_jsr_methods` | the regression: status is `Verified`, the plain method has real rows, `this` is a set bit in local 0, stack depth/oops track `aload_0` |
| `jsr_method_map_is_explicitly_unproven_not_fabricated_empty` | zero rows, `oop_map_at` `None` at every pc, `safe_for_fast_path == false`, veto is `Subroutine` |
| `subroutine_class_type_map_indices_track_class_methods` | abstract/native methods keep their index slot; by-name and by-index lookups agree |
| `failed_subroutine_class_publishes_no_maps` | a class that fails verification leaves the store `Unknown` |

Tests reserve `ClassId` 90_001..90_004: the type-map store is a process-wide
first-writer-wins side table, so tests that publish must not share ids under the
parallel harness.

---

## 2. Resolution caching audit (`classloading/src/resolution.rs`)

### 2.1 Inventory — what is actually cached

Four constructs; no `OnceLock`s and no global statics in this file.

| cache | key | scope | bound |
|---|---|---|---|
| `ResolutionCache` (`fields` / `methods` / `call_sites` / `condy`) | `(referring ClassId, cp index)` | one VM-global `RwLock` (`vm/src/vm/realms/class_realm.rs`) | 65 536 per map, FIFO |
| `LinkResolver` | `(ClassId, name, descriptor)` — **no** referring class | one VM-global `parking_lot::RwLock` | 131 072, CLOCK sweep |
| `InvokeCache` | `(caller ClassId, cp index, is_special)`; poly tier adds receiver `ClassId` | **per thread**, no lock | monomorphic tier unbounded; poly capped at 8/site |
| `RedefineGate` | per-entry snapshot of the declaring class's redefine counter | `Arc<AtomicU32>` shared with `ClassManager` | n/a |

### 2.2 What re-resolves on every call

* **`new` / `checkcast` / `instanceof` / `ldc` of a class constant** — there is
  no class-reference cache at all. `ResolutionCache` has field, method,
  call-site and condy maps and nothing for class refs. `Instruction::New` takes
  the class-manager read lock, allocates a fresh `String` for the class name from
  the constant pool, and runs `resolve_class_loader_aware` in full, **every
  execution**. This is the largest uncached hot symbolic path in the VM and it is
  outside this crate's files.
* **Loader-sensitive field access** — the loader-aware path consults
  `lookup_loader_initiated` *before* probing the cache and can reject an
  otherwise-valid hit, forcing a re-resolve for user-loader classes.
* **Invokes at `suppress_invoke_cache` sites** (loader-specific dispatch, user
  defining loader) never populate the invoke cache, so each call pays full
  `resolve_method_metadata` plus dispatch.

Cached and not re-resolved: `getfield`/`putfield`/`getstatic`/`putstatic`,
the four `invoke*` forms outside the suppressed sites, `invokedynamic` call
sites, and `ldc` of `CONSTANT_Dynamic`.

### 2.3 FIXED — memoized negative that survived invalidation

`LinkResolver::invalidate_class` retained every `ResolvedMember::NotFound` whose
*key class* was not the invalidated class:

```rust
ResolvedMember::NotFound => true,   // before
```

A negative is a statement about a hierarchy **walk**, not about one class.
`find_method_recursive` / `find_field_recursive` climb the superclass and
superinterface chain, so a `NotFound` cached at `(Sub, "m", "()V")` also asserts
that `Base` does not declare `m` — and the key records nothing about the classes
the walk crossed.

Live failure this produces, on a path that exists in this repo:

1. `Base` is still a synthetic stub without `m`.
2. JNI `GetMethodID(Sub, "m", "()V")` walks, fails, caches `NotFound` at key
   `(Sub, …)`.
3. `ClassManager::upgrade_synthetic_class` installs the real `Base`, which
   declares `m`, and fires the resolution-invalidate hook for `Base`.
4. The entry's key class is `Sub`, and `NotFound` carries no
   `declaring_class_id`, so the old `retain` kept it.
5. Every later `GetMethodID` returns the cached negative → null `jmethodID` →
   `NoSuchMethodError` for a method that now exists. Escapes: `clear()` (no
   production caller) or the CLOCK sweep at 128 K entries.

The same shape applies to JVMTI `RedefineClasses` adding a member to a
superclass.

**Fix:** drop every negative on any `invalidate_class`. `invalidate_class` has
no `ClassStore`, so it cannot ask whether the invalidated class is in the key
class's ancestry; dropping all negatives is the conservative direction — a
dropped-but-valid negative costs one re-walk and is semantically identical to a
miss, whereas a retained-but-stale one is permanently wrong. Invalidation is
rare (redefine / unload / synthetic upgrade) and the pass was already an O(n)
`retain`, so there is no new asymptotic cost.

Reachability note: the blast radius was limited to JNI `GetMethodID` /
`GetFieldID`, the only production writers of `NotFound`. The Java-reflection
consumers map `NotFound` to a cold miss and re-walk, so they were self-healing.

Tests: `negative_cached_under_subclass_is_dropped_when_ancestor_changes`
(regression), `unrelated_positive_survives_invalidation` (no over-eviction), and
`link_resolver_caches_and_invalidates` updated — it previously asserted the buggy
"keep the NotFound entry" behaviour.

### 2.4 Documented, not fixed — memoized `None` in `ResolvedMethod.native_target`

This is the same "memoized `None` is permanent" shape the sibling found in the
native registry, in a second place. `ResolvedMethod` carries
`native_target: Option<NativeCallback>` and `native_kind: Option<NativeKind>`.
The producer (`vm/src/runtime/interpreter.rs`, `resolve_method_metadata`)
collapses the absent case with `.unwrap_or((None, None))` after one
`NativeMethodRegistry` probe, and the result is stored in `ResolutionCache`.
`None` therefore means "the registry said no", memoized for the entry's
lifetime; nothing re-tries it, because `ResolutionCache::invalidate_class` fires
only on redefine / unload / synthetic upgrade, none of which registering a
native triggers.

**Currently sound, by accident of timing**: the registry is fully populated
during `vm_init` and never grows afterwards (JNI `RegisterNatives` writes a
separate table, consulted independently). It becomes a live bug the moment any
lazy or deferred native-registration pass is added — every method resolved
before that pass permanently believes it has no native. The field now carries a
warning to that effect; the structural fix (a registry epoch compared on cache
hit, or invalidating from the registration path) needs the producer, which is
not this crate. See [§5.2](#52-interpreterrs--native_target-negative-and-a-dead-invalidation-prong).

### 2.5 Documented, not fixed — a dead invalidation prong

`ResolutionCache::invalidate_class` advertises two-pronged invalidation: by key
class, and by resolved declaring class. **Prong 2 is live for `fields` and dead
for `methods`.** `ResolvedField`'s producer stores the true declaring class found
by the superclass walk, so a field resolved *into* a redefined superclass is
correctly evicted. `ResolvedMethod`'s only producer sets
`declaring_class_id: current_class_id` — the *referring* class, identical to the
key — so the second conjunct can never eliminate an entry the first did not. The
"sees through proxy/intermediate classes" behaviour the doc comment describes
does not occur for methods. Not paperable-over inside this crate: a same-crate
change cannot make the field carry information the producer never wrote.

Also noted, not owned: `InvokeCache` entries are not flushed by the invalidate
hook at all — they rely on `RedefineGate` lazy self-eviction, and on the
*unload* path the class's redefine counter is removed from `ClassManager` while
the `Arc<AtomicU32>` survives in the gate and is never bumped again, so unload
staleness is not detected by the gate. And the two `clear()` callers
(`libcratonvm`, `vm-cli`, post-`initPhase1` recovery) clear only
`vm.main_thread.invoke_cache`; other threads' per-thread caches are untouched.

---

## 3. Classpath scan cost (`classloading/src/class_path.rs`)

### 3.1 Structure

Per-entry lookups are already properly indexed and built once: `entry_index`
(`FxHashSet` per archive), `classes_cache` / `class_to_module` /
`resource_to_modules` for JMOD and jimage, `versions_cache` for multi-release
JARs, `signer_cache` (`OnceLock`, so the PKCS#7 verify runs once per archive),
and the bounded FIFO `canonicalize_cache`. No lookup path opens an archive or
re-reads a central directory. That half of the work is done.

What is **not** indexed is the level above: there is no cross-entry index, so
every public lookup is O(num_entries), and a *miss* always costs the full walk
(a hit short-circuits at the entry that has it). `ClassManager` then multiplies
this by three by concatenating bootstrap, extension and application classpaths
with no early exit.

### 3.2 FIXED — glob parsed once per call instead of once per candidate

`matching_resource_entry_names` filtered with `resource_name_matches_simple_glob`,
which re-ran `simple_resource_glob` — two `contains` scans, an `rfind`, and an
`is_safe_resource_name` prefix check that is itself six more substring scans —
**for every candidate name**, on a loop-invariant string.

The hottest reachable caller is `ClassLoader.getDefinedPackage`
(`native-builtins/src/lang_class.rs`), which builds `"{pkg}/*.class"` and calls
`find_all_resource_urls`; that is reachable from `Class.getPackage()`, i.e. once
per class for ByteBuddy/Spring/Jackson-style frameworks. Per call it iterates
every entry name of every archive on all three classpaths — so the re-parse ran
O(total entry names across all jars) times per `getPackage()`.

Now parsed once, with the predicate factored into
`resource_name_matches_parsed_glob` so the two forms are provably the same
function. Behaviour is identical, including the unparseable-glob case (the
per-candidate predicate returned `false` for everything, yielding an empty vec;
the early return is the same answer without the walk). Pinned by
`matching_resource_entry_names_agrees_with_per_candidate_predicate` and
`parsed_glob_predicate_matches_unparsed_form`.

### 3.3 FIXED — existence probes no longer inflate the entry

Two call sites answered a yes/no question with
`find_in_archive(archive, name).is_some()`, which **fully inflates the entry and
discards the bytes**:

* the JMOD branch of `find_all_resource_urls_impl` — a `getResource` hit on a
  JMOD paid a full deflate purely to decide whether to emit a URL string;
* the multi-release resolution inside the signed-JAR code-source path, which
  inflated each candidate `../../../apps/META-INF/versions/N/...` entry and then re-read the
  winner immediately afterwards — decompressing the same entry twice.

Both now use `archive_has_entry`, which takes the same lock and calls `by_name`
but drops the reader without reading compressed data. Debug builds, where
deflate is very slow, benefit most. Pinned by
`archive_has_entry_agrees_with_find_in_archive` (uses a Deflated entry so the
two paths genuinely differ in work done).

### 3.4 NOT fixed — no negative memo for class or resource misses, and why

`class_path.rs` memoizes no negative anywhere. Every miss re-walks every entry,
forever, on all three classpaths. The only negative memo in the chain is
`class_manager.rs`'s `synthetic_upgrade_absent`, which covers only the
synthetic-stub upgrade probe — the narrow fix for the JAXB incident, not a
general one. Resource misses are worse than class misses because the two
`getResource` implementations fall back from `find_resource` to
`find_all_resource_urls` (or vice versa), costing **two** full walks per miss;
`ServiceLoader` does the same inside per-provider loops.

**Deliberately not fixed in this session**, because the obvious fix is unsound:
a miss is not stable. `ClassPathEntry::Directory` probes the live filesystem,
and classes and resources do legitimately appear there at runtime (compiler
output, agent-generated classes, exploded webapp reloads). A blanket
absent-memo would turn those into permanent, silent `ClassNotFoundException`s —
the same class of bug as §2.3, one layer down, and much harder to attribute.

The sound design, for whoever picks it up: memoize the absent verdict **per
immutable entry kind only** (`JarFile` / `NestedJar` / `JmodFile` / `JImageFile`
/ `NestedDirectory`), and always re-probe `Directory` entries. A classpath with
no `Directory` entries — the bootstrap `jmods`/`lib/modules` case, which is most
of the walk — then answers a miss in O(1). `ClassPath::add_path(&mut self)` is
the only mutation point, so invalidation is a single `clear()` there. This is a
larger change than is safe to land unbuilt in a 287 KB file; it should be done
with the ability to run the classloader test suite.

### 3.5 NOT fixed — remaining costs, ranked

1. `getDefinedPackage` → glob `find_all_resource_urls` per `Class.getPackage()`:
   §3.2 removes the per-candidate re-parse, but the call still does `read_dir`
   on every `Directory` entry and a full filter+clone+sort of every archive's
   entry index. The right fix is a memoized per-package index, and it belongs in
   the caller (`native-builtins`), not here.
2. `simple_resource_glob(name)` is re-evaluated inside the per-entry loops at
   ~18 sites in `find_resource_impl` / `find_all_resource_bytes` /
   `find_all_resource_urls_impl` (N_entries redundant parses per call rather
   than N_names). Mechanical to hoist; left alone this session because 18
   coordinated edits in an unbuildable file is a poor risk trade for a
   second-order win once §3.2 landed.
3. Per-entry `exists()` stat plus a fresh `PathBuf` join on every `Directory`
   probe, on all of `find_class` / `find_resource` / the glob path.
4. `find_all_resource_bytes` is not wrapped in `diag_resource_call_wrapper`
   unlike its two siblings — a diagnostic blind spot on the `ServiceLoader` hot
   path.
5. JAR-URL string normalization (`to_string_lossy().replace('\\', "/")` and
   friends) rebuilt per hit; could be precomputed once per entry at load.

---

## 4. Access control (`classloading/src/access_control.rs`)

### 4.1 The caching question, answered

**No cache bypasses an access check** — and the reason is worse than the
question anticipated: on the bytecode resolution path there is no member access
check to bypass.

The caching itself is correctly shaped. Every live resolution cache is keyed on
the *referencing* class (`ResolutionKey = (ClassId, u16)`,
`InvokeCacheKey = (caller ClassId, cp index, is_special)`,
`PromotedInvokeKey` adding the receiver). JVMS 5.4.4 resolution is defined per
(referencing class, symbolic reference), so a hit is by construction the same
accessor the check was performed for; re-running it would be redundant. The JPMS
check that *is* wired (`check_module_access_by_id`) runs on the miss path, i.e.
before the value is cached — the correct order. `Instruction::New`'s
`check_class_access` is unconditional on every execution, not memoized at all;
its JIT prescans cache the verdict but key it on the compiling class with a
fail-closed sentinel fallback.

The bug shape to watch for is a globally-keyed resolved member reused from a
*different* referencing class without re-checking. It is not present. The one
globally-keyed member cache, `LinkResolver`, stores no access verdict and serves
only JNI `GetMethodID`/`GetFieldID`, where the spec does not mandate the check.
**Latent hazard:** `vm/src/runtime/lockfree_resolve.rs`'s
`SharedResolutionState::global_methods` / `global_fields` use an accessor-free
key (class + member + descriptor + loader epoch). They have no production
callers today. That key shape must never be adopted for anything carrying an
access verdict.

### 4.2 The real finding — JVMS 5.4.4 member access is unenforced

`check_field_access` and `check_method_access` have **zero production call
sites**, as do all three `_with_modules` wrappers and (transitively)
`are_nestmates` and `receiver_ok_for_protected`. They are fully implemented,
well tested, and never called.

Consequence: at `getfield` / `putfield` / `getstatic` / `putstatic` /
`invoke*`, a hand-written class file naming another class's `private` or
package-private member resolves and executes. The verifier does not compensate:
`IllegalAccessError` is constructed nowhere outside this module. Class-level
access is enforced only for `new` — not for `checkcast`, `instanceof`, `ldc`,
`anewarray`, or the owner class of a field/method reference. (Reflection is a
separate subsystem with its own correctly-wired check in
`native-builtins/src/lang_class.rs`; `setAccessible` and the self-class
exemption there are spec-legitimate.)

The checks themselves are correct where they exist. The protected rule
implements **both** JVMS clauses — accessor must be a subclass of the declaring
class, and the receiver's static type must be the accessor or a subtype — not
just class-level access dressed up, and the classic sibling-receiver denial is
covered by existing tests.

Wiring this up requires `vm/src/runtime/interpreter.rs`; see
[§5.3](#53-interpreterrs--wire-up-jvms-544-member-access).

**A trap for whoever does that wiring, now pinned by a test.** Passing
`receiver: None` satisfies the receiver-subtype clause *vacuously*. A wirer who
finds the static receiver type inconvenient to thread through and passes `None`
gets clause 1 only, and cross-package protected access that JVMS forbids is
allowed. `protected_receiver_none_is_vacuous_not_a_check` asserts the divergence
explicitly so it cannot be discovered by accident later. Plumbing the receiver
type through `getfield`/`invokevirtual` is part of the job.

### 4.3 Fail-open paths, classified

Documented in-file rather than changed, because none can be tightened without a
boot-path run this session could not do:

| site | verdict |
|---|---|
| empty module registry → allow | **correct** — classpath-only mode has no module boundary |
| unnamed module involved → allow | **spec-mandated** JPMS classpath compatibility |
| array target (`[`-prefixed) → allow | **correct**, matches the JDK |
| target package empty → allow | pragmatic hole, already documented in-file; reachable only by a class the VM itself labels as being in a named module with a default package |
| `check_module_access_by_id`: either `ClassId` not in the store → allow | **fail-open**, now called out in the doc comment. Both classes are normally resident by this point, so not currently exploitable, but it is a silent allow rather than an error |

`CRATONVM_DBG_ACCESS` is diagnostics only (an `eprintln!` on the deny path); it
cannot flip a verdict. There is no trusted-caller bypass, no `unsafe`, and no
other env var in this file.

---

## 5. Cross-owner requests

### 5.1 `bytecode_verifier.rs` — don't record a stale frame

**Owner: whoever holds `classloading/src/bytecode_verifier.rs` (the `type_maps`
sibling). Highest priority item in this document — it is a potential
missed-root / oop-scan-of-a-non-oop bug in code that is about to be trusted by
the GC.**

`verify_method`'s linear StackMapTable walk calls
`type_maps.record(pc, &current_frame)` unconditionally after the
unreachable-code guard. There are two pcs where `current_frame` is a stale frame
from an unrelated predecessor:

1. **Pre-Java-7 class that ships a `StackMapTable`.** The unreachable-code guard
   is `if !verified && requires_stack_map && …`, and `requires_stack_map` is
   `version.major >= 51`. For major ≤ 50 it never fires, so the walk continues
   into dead code after `goto`/`return`/`athrow` still carrying the previous
   instruction's frame. That pc can be a live branch target at runtime.
2. **A handler pc also reachable by fall-through.** The handler frame is
   installed only under `if !verified`. When control does fall through,
   `current_frame` describes the fall-through edge; the exception edge can enter
   the same pc with different locals and `[throwable]` on the stack, and the two
   are never merged.

Type-*checking* against a stale frame is a pre-existing laxity of that walk;
*recording* one manufactures a row the GC will act on. The fix mirrors what
`verifier.rs::verify_method_typestate` now does — an `authoritative` flag beside
`verified`:

* initialise `true` (pc 0 uses the initial frame);
* set `true` when a declared frame is adopted, and when a handler frame is
  installed;
* set `false` at a handler pc reached with `verified == true` and no declared
  frame;
* after each instruction, `authoritative &= result.falls_through && next_pc < bytecode.len()`;
* record only when `authoritative`, else `walk_complete = false`.

`verify_by_inference` needs no change: it serializes a settled fixpoint that
includes merged handler entries.

### 5.2 `interpreter.rs` — `native_target` negative, and a dead invalidation prong

Two independent items in `vm/src/runtime/interpreter.rs`:

1. `resolve_method_metadata` memoizes "no native exists" via
   `.unwrap_or((None, None))` into `ResolvedMethod`, and nothing re-tries it.
   Sound only while the native registry stops growing after `vm_init`. If lazy
   or deferred native registration is ever added, either compare a registry
   epoch on `ResolutionCache` hit or invalidate the resolution cache from the
   registration path. (§2.4.)
2. The same function sets `declaring_class_id: current_class_id` — the referring
   class — so `ResolutionCache::invalidate_class`'s declaring-class prong is a
   no-op for methods. Setting the true declaring class found by the resolution
   walk (as the field path already does) makes redefine see through
   proxy/intermediate classes as the doc comment claims. (§2.5.)

### 5.3 `interpreter.rs` — wire up JVMS 5.4.4 member access

`check_field_access` / `check_method_access` exist, are correct, and are never
called (§4.2). They belong on the resolution miss path, next to the existing
`check_module_access_by_id` calls — i.e. *before* the result is written to
`ResolutionCache`, so the cached value is the already-checked one and hits stay
correct without re-checking (the key already contains the accessor).

The receiver argument must be the **static type** of the receiver expression,
not `None`, or the cross-package protected clause is vacuous (§4.2). For
`getfield`/`invokevirtual` that is the class named by the field/method ref's
owner as narrowed by the verifier's type state; `None` is correct only for
genuinely static access.

Do not adopt `lockfree_resolve.rs`'s accessor-free `ResolutionKey` for anything
carrying an access verdict.

### 5.4 `vm` — mark skipped verification

Carried forward from the sibling's doc because it is still open: at the point
where `vm` decides to skip verification for a class (the
`config.skip_verification` / `verifier_skip_eligible` gate), call
`cratonvm_classloading::mark_class_verification_skipped(class.id)`. Without it
the status is `Unknown` instead of `Skipped` — equally conservative, just less
diagnosable.

### 5.5 `native-builtins` — memoize `getDefinedPackage`

`ClassLoader.getDefinedPackage` runs a glob `find_all_resource_urls` per
`Class.getPackage()` (§3.5). Even after §3.2 it costs a `read_dir` per directory
entry plus a full filter+clone+sort of every archive's entry index. A memoized
package → exists map in the caller removes it entirely; `class_path.rs` cannot,
because it does not know the result is being asked repeatedly for the same
package.

---

## 6. Summary of changes landed

| file | change |
|---|---|
| `verifier.rs` | publish `ClassTypeMaps` from the subroutine-bearing class path; real maps for its non-`jsr` methods; explicit row-less `FastPathVeto::Subroutine` entry for `jsr` methods; `authoritative`-frame guard so no stale row is recorded; module docs record the `jsr`/`ret` decision and its reasoning; 4 tests |
| `resolution.rs` | **bugfix**: drop memoized negatives on `LinkResolver::invalidate_class`; document the `native_target` memoized-`None` hazard and the dead methods invalidation prong; 2 new tests, 1 corrected |
| `class_path.rs` | parse resource globs once per call instead of once per candidate; add non-inflating `archive_has_entry` and use it at the two existence-only probes; 3 tests |
| `access_control.rs` | module STATUS section recording that member access is unenforced and that the caching is correctly keyed; fail-open note on `check_module_access_by_id`; 1 test pinning the vacuous-receiver trap |
