# Additional `JDK-ONLY-WAVE2` findings not in the original wave-2 inventory

**Status:** OPEN — JDK-only wave-2 work items, filed 2026-07-31, re-verified
against the re-landed tree the same day. These were found while verifying the
eleven inventoried findings. They are recorded here so wave 2 does not
rediscover them, and because two of them (§1 and §4) are the same defect shape
as items that *were* inventoried, in code nobody was looking at.

Each item states where the marker lives so it can be re-read in full. **Anchor
on the marker text, not the line number** — the re-land moved most of these.

**Four items moved materially in the re-land** and are flagged inline: §1 (the
JIT's inline caches are now closed-by-refusal under `JdkOnly`), §3 (the JIT's
by-name native fast paths are policy-checked and counted, and `build_helpers`
now publishes the policy), §12 (all three documentation gaps are closed) and
§13 (three of five stale doc paths remain, and the count is per-occurrence not
per-path).

---

## 1. The JIT has the same missing-`NativeKind` hole as the interpreter's invoke cache — twice — but wave 1 bought time with a blanket refusal

`jit/src/lib.rs` 4773 (`JitMICSlot`) and 5092 (the polymorphic/megamorphic
variant):

> **JDK-ONLY-WAVE2 — this cache stores an entry pointer and no `NativeKind`.**
> `cached_entry_ptr` is a raw address that generated code `CALL R11`s on a
> class-id guard hit. When the target is a native/builtin trampoline the slot
> has kept the callback and thrown away its kind, so **nothing on the hit path
> can re-check policy**.

and

> **same missing-`NativeKind` shape as `JitMICSlot`.** `entry_ptrs[i]` and
> `mega_entry_ptrs[i]` are raw addresses with no kind beside them, so a hit
> cannot re-check policy.

**What changed.** The original filing said this was *worse* than the
interpreter's version because generated machine code jumps to a raw address with
no opportunity to check anything. Under `JdkOnly` that is no longer true:
`jit_entry_publishable` (`jit/src/lib.rs` 5970) ends with

```rust
if jit_is_jdk_only() {
    record_jdk_only_ic_native_refusal();
    return false;
}
```

so an unowned (i.e. native/builtin) entry is never published into an MIC or PIC
slot in strict mode, and the site falls back to `jit_invoke_virtual_mic` /
`jit_invoke_dispatch`, which re-resolve under the policy. Both slot doc comments
record this, and the refusal is counted. The branch is reached only on an
inline-cache **miss**, which already takes two mutexes, so `Compatible` is
untouched.

**Why it is still open.** The refusal is a tax, not a fix: every inline-cached
native call site in a `--jdk-only` run permanently takes the slow dispatch path.
And the missing kind still makes the census incomplete in `Compatible` mode. The
MIC doc explains why storing it was rejected for wave 1 and exactly what wave 2
must do:

> this struct is `#[repr(C)]` with three offsets (`0`, `8`, `16`) baked as
> immediates into emitted machine code in `jit/src/x64.rs`, `jit/src/ir_lower.rs`
> and `jit/src/runtime_lowering.rs`, and the only population site
> (`Self::update`) is called from `vm/src/jit/helpers.rs`, which this wave's
> owner cannot edit.

The PIC doc adds the shape: a parallel `needs_context`-style `AtomicU8` kind
array **appended at the TAIL**, since `CLASS_ID_OFFSETS` / `ENTRY_PTR_OFFSETS` /
`NEEDS_CONTEXT_OFFSETS` / `MEGA_*_OFFSET` are all emitted-code immediates.

Fix this together with
[cached invoke targets drop the `NativeKind`](cached-invoke-targets-drop-the-nativekind.md).
A `NativeKind` stored in `CachedInvokeTarget` that is dropped again when the JIT
installs an MIC slot buys nothing.

## 2. The JIT's compatibility policy is a process global, and so are the direct-helper addresses

`jit/src/lib.rs` 4018:

```rust
static JIT_COMPATIBILITY_MODE: std::sync::atomic::AtomicU8 = …;
```

with the marker on `set_jit_execution_policy` at 4027:

> this is process-global where §2 wants per-VM state. It is latched toward
> strict so it cannot mis-execute …, but a `Compatible` VM sharing a process
> with a `JdkOnly` one **silently loses the thin direct-call helpers**. The
> wave-2 replacement is to move the `*_DIRECT_FN` helper addresses AND this
> policy into a single per-VM struct.

The module comment above it (from ~3955) is honest about why the monotone latch
was chosen: the `*_DIRECT_FN` gates (`INTEGER_VALUE_OF_DIRECT_FN` et al., 4243
onward) are *already* process-global `AtomicUsize`s written by `build_helpers`,
so a per-VM policy could not gate them coherently anyway. Making both VM-scoped
together is the real fix. Contract §2 forbids process globals for this feature's
state precisely because of the repo's history of process-global native caches
leaking across VMs.

The same doc block carries an operational warning worth repeating: **do not call
`set_jit_execution_policy` from a unit test in this crate.** The latch is
irreversible and `mod tests` shares one test binary, so a test that latched
`JdkOnly` would make `jit_entry_publishable` start refusing native inline-cache
entries for every test that ran after it — an order-dependent failure.

## 3. The JIT-reachable native-dispatch paths that bypassed `resolve_dispatch` — mostly closed

`jit/src/lib.rs` 4178 carries a `JDK-ONLY-NOTE` enumerating dispatch sites the
JIT crate cannot fix because they live in `vm/src/jit/helpers.rs`. It now has
**six** items, not five; items 5 and 6 are explicit "assessed, no action"
verdicts rather than gaps. Condensed, with current status:

1. `jit_invoke_dispatch` (`vm/src/jit/helpers.rs` 6589) and
   `jit_invoke_virtual_mic` (8948) — **THE** JIT dispatch path for every
   compiled invoke that is not inlined, intrinsified or directly bound. The note
   asks for `resolve_dispatch` routing and `record_invocation`. **Partially
   closed:** both now reach natives through `admit_jit_fast_native`, which does
   both. Whether *every* arm of both functions does is unaudited — treat this as
   open until someone walks them.
2. JIT-only native fast paths that resolved natives by name on their own: the
   `java/lang/ClassLoader` `getResource*`-with-null intercept,
   `hashmap_native_callback` (8387), `matcher_native_callback` (8463),
   `stringbuilder_native_callback` (8504). **Closed.** All now call
   `admit_jit_fast_native` (6095), which resolves a `NativeMethodId`, pairs the
   fast-path callback with the *registered* kind, routes the pair through
   `resolve_native_dispatch_wave1`, and hands back an id for
   `count_jit_native_dispatch` (6142). The admission is at **cache-fill** time,
   so the per-dispatch cost is one `Option` test plus one relaxed `fetch_add`.
   Its own comment states the rule that makes it correct: *"No registration at
   all (`id == None`) yields `native == None`, the resolver answers `None`, and
   the fast path is refused … a VM-side reimplementation the registry has never
   heard of cannot be policy-audited, so it must not stand in front of real
   bytes."*
   **One residual:** `matcher_native_callback_uncached` (8439) keeps the old
   policy-free body for the single caller that re-decides on every dispatch —
   `jit_invoke_virtual_mic`'s exact-receiver `Matcher` leaf, which hoists the
   JDK-only question to one `dispatch_policy(vm).is_jdk_only()` gate (9267)
   instead of paying policy per call. Under `JdkOnly` the leaf is **skipped
   wholesale** and `Matcher.find` falls through to the policy-checked generic
   tail, so it contributes nothing to the acceptance criterion by construction;
   under `Compatible` it is **census-uncounted**. Its `JDK-ONLY-WAVE2` marker
   (`helpers.rs` 9258) names the fix — a per-call-site memo of the admitted
   `(callback, NativeMethodId)` decision, keyed on `info_ptr` or on eight
   `NativeCallSite` cells — which would both count the edge and give the strict
   path its documented 19x fast route back instead of leaving it merely safe.
3. `build_helpers` must call `set_jit_execution_policy` before the first
   compilation, and should skip the `set_*_direct_fn` registrations under
   `JdkOnly`. **Closed** — `helpers.rs` ~11660 resolves the policy through
   `process_vm()`, publishes it, and gates the `*_DIRECT_FN` registrations on
   the resulting `jdk_only` flag. `None` (a unit test with no VM) leaves the
   latch untouched, which the JIT already treats as `Compatible`.
4. `cp_elidable_init_resolver` — eliding an `<init>` that is shadowed by a
   registered native skips the native (the known
   `jit-elidable-ctor-must-check-native-shadow` defect). **Still open**; the
   shadow check is on the VM side and needs the same `resolve_dispatch`
   treatment.
5. `INDY_STRING_CONCAT_FN` — assessed and deliberately **not** gated; a
   `StringConcatFactory` *bootstrap* bridge, not a native-method dispatch, and
   the interpreter reaches the same bridge for the same sites, so gating it
   would move the call without changing the policy answer while perturbing
   `CompiledMethod::has_indy_trap` (an OSR-correctness input).
6. `MONITOR_ENTER_DIRECT_FN` / `MONITOR_EXIT_DIRECT_FN` — VM monitor services,
   not registered natives. Not a dispatch site; no action. (`helpers.rs` 11695
   notes both exclusions match this list.)

Contract §11 requires *"every strict-mode native dispatch"* to be an
`ACC_NATIVE` bridge, a reviewed VM service or a proven intrinsic, and §4's
counters to be complete. Items 1 and 4 are what still stand between the tree and
that claim for a JIT-compiled frame.

## 4. Seven JIT "thin direct call" ladders bake a native reimplementation into emitted code

`jit/src/lib.rs` 9894 carries the full marker; six more sites reference it
(9934, 10145, 10174, 10204, 10242, 10256):

> hard-coded (class, method, descriptor) exception list. Seven of these ladders
> bake a thin VM-side reimplementation of a registered native straight into the
> emitted CALL, bypassing `vm_exec::resolve_dispatch` entirely. Wave 1 gates them
> on policy via `direct_native_helper`; wave 2 should replace the name matching
> with a resolver callback that asks `resolve_dispatch` whether this triple is
> an approved `NativeKind::Intrinsic`, and delete the literals. **Do NOT delete
> the list before that resolver exists — every entry here is a measured hot
> path.**

The seven, verified 2026-07-31:

| Marker | Triple | Helper |
|---|---|---|
| 9894 | `java/lang/StringLatin1.toLowerCase(Ljava/lang/String;[BLjava/util/Locale;)…` | `STRING_LATIN1_LOWER_DIRECT_FN` |
| 9934 | `java/lang/Integer.valueOf(I)` | `INTEGER_VALUE_OF_DIRECT_FN` |
| 10145 | `java/lang/Integer.intValue()I` | `INTEGER_INT_VALUE_DIRECT_FN` |
| 10174 | `java/util/concurrent/ConcurrentMap.get(Object)` | `CONCURRENT_HASHMAP_GET_DIRECT_FN` |
| 10204 | **`java/lang/String.toLowerCase(Ljava/util/Locale;)`** | `STRING_LOCALE_LOWER_DIRECT_FN` |
| 10242 | `java/util/HashMap.put` / `java/util/Map.get` recognition head | — |
| 10256 | the `put` / `get` bind under it | `HASHMAP_PUT_DIRECT_FN` / `HASHMAP_GET_DIRECT_FN` |

Two of the seven are in the `String` family, which makes the JIT a **third**
location for the forced-native `String` policy documented in
[the forced-native `String` policy](forced-native-string-policy-two-lists-that-disagree.md).
Be precise about which: 10204 is `java/lang/String` itself and `toLowerCase` is
one of the 21 names on `check_override`'s positive list *and* is excluded by
`force_native_over_real_jdk_bytecode`'s seven-pair whitelist — three paths,
three answers. 9894 is `java/lang/StringLatin1`, the helper the real `String`
bytecode delegates to, which is a fourth spelling of the same policy rather than
a fourth answer.

Wave 1 did gate these on policy (`direct_native_helper`, `jit/src/lib.rs` 4163,
refuses to bind under `JdkOnly` at 4170 and records a `NativeShadowsBytecode`
violation), so this is the best-behaved of the four JIT items — but the literals
remain. `vm/src/jit/helpers.rs` 7540 carries the matching marker on the helper
*bodies*, explaining why they carry no internal check: they are gated twice, both
gates upstream, and *"adding a third, per-invocation check inside these bodies
would put a policy read on the hottest boxing/collection paths in the VM to
defend against a state that cannot occur. If either gate above is ever removed,
this comment is the reason these bodies look unguarded."*

## 5. `jit-api`'s `force_native_cache` memoizes a hard-coded dispatcher, and the memo is not policy-qualified

`jit-api/src/lib.rs` 204:

> this cell memoizes the *answer* of a ~1400-line hard-coded
> class-name/method-name dispatcher (`force_native_over_real_jdk_bytecode`)
> whose whole purpose is to make a registered native win over concrete real-JDK
> bytecode — the exact inversion §1 rule 4 forbids under `JdkOnly`. The list is
> a wave-2 removal and must NOT be deleted this wave. **What must change here:
> the memoized `bool` has to become policy-qualified**, because a `true`
> memoized under `Compatible` is not a valid answer under `JdkOnly` and this
> cell cannot tell the two apart. Simplest correct shape is to store the
> `CompatibilityMode` alongside the bool (`OnceLock<(bool, u8)>`) and re-derive
> on a mode mismatch; the mode is fixed per VM, so the compare is free. Not done
> in wave 1 because the ~38 struct literals of this type spell the field
> `std::sync::OnceLock::new()` and live in four crates owned by four different
> agents.

Two corrections to the marker's own text, worth knowing before acting on it:

* **The function is 2,325 lines** (`vm/src/runtime/interpreter/invoke.rs`
  6963–9288), not ~1,400. The marker also cites it as living in
  `vm/src/runtime/interpreter.rs`; it is in the `invoke` submodule.
* The memo turns *"O(~55 string comparisons) on every cached dispatch"* into an
  O(1) read. It is a perfectly good optimisation of a list that should not
  exist; when the list goes, so does the cell. The policy-qualification work is
  only needed if the list outlives wave 2.

The redefine-dependent wrapper (`should_force_registered_native_over_bytecode`)
is deliberately **not** cached here — it depends on mutable per-class redefine
state — so it is still re-evaluated on every hit.

## 6. `JNI_NATIVE_METHODS` is a process global, and JNI dispatches are uncounted

`vm/src/native/jni.rs` 4650:

```rust
static JNI_NATIVE_METHODS: std::sync::LazyLock<parking_lot::RwLock<HashMap<u64, usize>>> = …;
```

> **JDK-ONLY-WAVE2:** this is a **process global**, which contract §2 forbids for
> this feature's state … It predates the feature and holds only `dlsym` results,
> so it is not JDK-only state; it should nonetheless move into
> `SharedVm::natives` alongside `native_methods` so two VMs in one process cannot
> see each other's `RegisterNatives`. NOT moved this wave — it is touched by
> `vm_exec.rs`, owned by another agent.

The same doc block records a census consequence: JNI-registered natives are not
in the §4 census, because `record_invocation` keys on a `NativeMethodId` that
only `NativeMethodRegistry` issues. `synthetic_stub_invocations` stays exact (a
stub cannot be registered here), but **`bridge_invocations` under-counts genuine
JNI bridges**.

## 7. Pre-existing: JNI `Call*Method` helpers swallow `ExceptionThrown`

`vm/src/native/jni.rs` 1268, flagged as *"pre-existing, orthogonal to this
feature, not fixed here"*:

> the JNI `Call*Method` helpers silently swallow
> `MethodCallFailed::ExceptionThrown`, so a Java exception thrown by a
> JNI-initiated call never becomes a pending JNI exception. Fixing it would
> alter `Compatible` behaviour, which this wave may not do.

Wave 1 added `jni_surface_jdk_only` (1274, called from 1356, 1401, 1436 and
5562) to intercept exactly one value — `VmError::JdkOnly` — so a refusal behind
a `CallObjectMethod` becomes a pending JNI exception instead of vanishing.
Everything else still takes the old path (`Err(_) => None`). This is a real,
separate JNI-correctness bug that happens to have been noticed here.

One loose end the code itself flags: `raise_jdk_only_violation` throws
`java/lang/InternalError`, with an `ORCHESTRATOR:` note saying it should follow
whatever Java-visible type `--explain-jdk-only`'s rendering settles on. That
decision has not been made.

## 8. The interpreter substitutes a *different class's* native for unresolvable interface calls

`vm/src/runtime/interpreter.rs` 6241:

```rust
let canonical: &'static str = match &*class_name_owned {
    "java/util/Set" | "java/util/Collection" => "java/util/HashSet",
    "java/lang/Iterable"                     => "java/util/ArrayList",
    "java/util/List"                         => "java/util/ArrayList",
    "java/util/Map"                          => "java/util/HashMap",
    "java/util/Iterator"                     => "java/util/HashMap$KeyItr",
    _ => "",
};
```

> **JDK-ONLY-WAVE2: hard-coded class-name exception list.** This substitutes a
> *different class's* native for an unresolvable interface call — a compatibility
> substitution in the §1 sense, and one that is **silent**: the receiver is not
> an instance of `canonical`. What should replace it: a real interface-method
> resolution (JVMS §5.4.3.4 `selectMethod` over the receiver's runtime class),
> with the `ClassOrigin` of the receiver deciding whether a shim receiver is even
> legal. Under `JdkOnly` real class bytes make every one of these interfaces
> resolvable, so the map should become unreachable rather than conditional. NOT
> deleted this wave — removing shim mappings has regressed real-JDK boot before.

This belongs in the "silent wrong behaviour" tier alongside the layout item —
the VM runs `HashMap$KeyItr`'s native against a receiver that is not one.

## 9. The three `redefine_immune_*` predicates take a §1.4 decision outside `resolve_dispatch`

`vm/src/runtime/interpreter.rs` 5869:

> the three `redefine_immune_*` predicates are hard-coded class/method-name
> exception lists (defined in `vm/src/runtime/interpreter/invoke.rs`, not owned
> here) … They encode "this native keeps winning even over **instrumented**
> bytecode", which is a §1.4 shadow decision taken outside `resolve_dispatch`.
> What should replace them: `NativeKind` — exactly `Intrinsic` should be
> redefine-immune, and everything else should yield to redefined bytecode, with
> no name list at all. NOT deleted this wave; the lists gate real
> Mockito/ByteBuddy behaviour.

The three are `redefine_immune_reflection_native`,
`redefine_immune_string_builder_native` and `redefine_immune_path_native`, all
`&&`-ed into the `class_redefined` guard at that site.

## 10. The wave-1 name-only dispatch adapter hard-codes `compat_native_wins = true`

`vm/src/runtime/interpreter.rs` 5633, inside the adapter that funnels the
name-only `find` call sites through `resolve_native_dispatch_wave1`:

> hard-coded `true` reproduces the pre-§7 "a registered native unconditionally
> wins here" of the `find` calls this replaces. Wave 2 replaces it with the real
> per-site compatibility verdict once `force_native_over_real_jdk_bytecode` and
> the forced-native `String` list … are unified.

Worth flagging because it is one of two places where wave 1 deliberately
*encoded* the old behaviour as a constant; the other is
`jdk_only_admit_jit_fast_native` (`vm/src/jit/helpers.rs` 6064), which passes
`true` with a comment explaining that reaching that point *is* the site's
pre-existing verdict. Both will read as bugs to anyone who finds them without
the marker. Note that this adapter *does* call `record_invocation(id)` on the
admitted path, so unlike the cached paths it is counted.

## 11. `check_override`'s exception chain is 217 disjuncts over ~2,650 lines

`vm/src/vm/vm_exec.rs`, marker at 17582, chain from 17595 to 20244 (2,650
lines; 217 top-level `||` disjuncts, several of which are multi-arm `matches!`
blocks covering many triples each):

> the `check_override` chain header. Of the ~250 disjuncts below, exactly ONE
> survives contract §7: `method.is_abstract()`, which is §7 step 3b (no `Code`,
> so a registered native is the only thing there is to run — **the documented
> deviation in `resolve_dispatch`'s banner**). Every other disjunct is a
> class-name exception saying "prefer our native over the real JDK's concrete
> bytecode", which is precisely what §1.4 forbids. What must replace the whole
> chain: nothing — under `--jdk-only` `resolve_dispatch` step 3 returns
> `Bytecode` for all of them. Removing them under `Compatible` is a separate,
> per-family exercise; each entry is load-bearing for a real boot today.

This is the largest single wave-2 deletion in the tree and the one most directly
blocked by
[`NativeKind` is ambient](native-kind-is-ambient-and-defaults-to-syntheticstub.md):
reclassifying 217 disjuncts' worth of entries is only safe once each native's
kind is a stated fact rather than an inherited one.

### 11a. `resolve_dispatch`'s deliberate deviation from §7 — a known, justified departure, not a bug

The marker above leans on it, so it needs to be written down where a wave-2
engineer will find it. `resolve_dispatch` (`vm/src/vm/vm_exec.rs` 178) does
**not** implement contract §7's four steps literally. Between step 3 and step 4
it inserts one extra branch, and the function's own banner documents it under
the heading *"Deliberate deviation from the literal §7 order"*:

> Between steps 3 and 4 this function inserts: *a method with no `Code` but a
> registered native dispatches to that native.* The literal order sends every
> `Code`-less method straight to `Reject(MissingImplementation)`, which would
> reject every interface-level bridge the VM boots on — `Path.getFileSystem`,
> `Iterator.hasNext` on a synthetic wrapper, `Enumeration.hasMoreElements`, the
> whole `FileSystemProvider` surface. Nothing would start in *either* mode, so
> the deviation is not a strictness concession, it is the difference between a
> resolver that can be turned on and one that cannot.
>
> It does not weaken §1.4. §1.4 forbids a native **shadowing** bytecode, and
> this branch is only reachable when `method.code()` is `None` — there is no
> bytecode to shadow. `SyntheticStub` is still refused here under `JdkOnly`, so
> §1.3 is enforced on this branch too.

The code matches the banner: the branch re-checks `strict && kind ==
SyntheticStub` and rejects before returning `NativeBridge(cb)`.

**Do not "fix" this to match §7's literal wording.** It is a reviewed,
justified, self-documented deviation, and reverting it would prevent the VM from
booting in both modes. The banner also names the right long-term shape, which is
the actual wave-2 item: *"the abstract declaration should resolve to the
implementing class's method before reaching here, at which point step 3 covers
it and this branch can go."* That is a resolution-order change, not a policy
change.

## 12. Cross-file documentation gaps flagged from code — all three CLOSED

The original filing listed three. All three were closed during the re-land; they
are kept here so nobody re-opens them from a stale report.

* `libcratonvm/src/lib.rs` — `docs/EMBEDDING.md` needed rows for
  `cratonvm_create_with_compatibility`, `cratonvm_compatibility_mode`,
  `cratonvm_compatibility_mode_supported`, the `CRATONVM_COMPATIBILITY_*`
  constants and the `CompatibilityMode` / `ExecutionPolicy` / `JdkMode`
  re-exports. **Closed** — `docs/EMBEDDING.md` now documents them, and the
  in-code marker is gone.
* `types/src/flag_groups.rs` — `vm-cli` should call
  `flag_groups::process_env_supersessions()` and print `Superseded::note()`.
  **Closed** — `vm-cli/src/main.rs` iterates it and prints each note. The single
  `SUPERSEDED` row (`flag_groups.rs` 833) is `CRATONVM_REAL=-stubs` → prefer
  `--jdk-only`, *"the environment token only filters the native registry, and
  cannot express the class-loading or dispatch half of the JDK-only contract"*.
* `types/src/error.rs` — `JdkOnlyViolation::render`'s `java.home` line read
  `<not recorded>` for six of seven variants because only
  `MissingBootClass::searched_image` carried a runtime image path. **Closed** —
  `render` (448) now resolves it through a `java_home()` helper and redacts it
  unless `verbose`. The signature is still `(jdk_feature, verbose)`, as
  contract §3 fixes it.

## 13. Stale documentation paths in load-bearing code comments — three paths, five occurrences

Several dispatch sites cite known-issue docs that have since been fixed and moved
to `docs/internal/fixed-suite-bugs/` with a `-FIXED` suffix. The cited paths no
longer resolve. The original filing listed three rows; the re-verification found
**five occurrences** of those three paths:

| Cited in code as | Occurrences | Actually at |
|---|---|---|
| `docs/known-issues/stringjoiner-synthetic-native-real-jdk-field-mismatch.md` | `native-collections/src/lib.rs:24522` | `docs/internal/fixed-suite-bugs/stringjoiner-synthetic-native-real-jdk-field-mismatch-FIXED.md` |
| `docs/known-issues/threadpoolexecutor-execute-npe-on-ctl-regression.md` | `vm/src/vm/vm_exec.rs:13501`, `vm/src/runtime/interpreter/invoke.rs:9998`, `:10149` | `docs/internal/fixed-suite-bugs/threadpoolexecutor-execute-npe-on-ctl-regression-FIXED.md` |
| `docs/known-issues/threadpoolexecutor-execute-dispatch-degrades-to-synchronous.md` | `vm/src/runtime/interpreter/invoke.rs:23059` | `docs/internal/fixed-suite-bugs/threadpoolexecutor-execute-dispatch-degrades-to-synchronous-FIXED.md` |

Note that the `StringJoiner` path is *also* cited inside `invoke.rs`'s
`real_protected_stub_class` comment, in a wrapped form
(`docs/known-issues/` + newline + `stringjoiner-…`) that a naive grep for the
full path misses. Search for the basename, not the path.

These comments are the *only* explanation for why several of the duplicated
dispatch sites in this directory exist. A wave-2 engineer who follows the link
and finds nothing is likely to conclude the workaround is obsolete. Repointing
them is a one-line-per-site fix and should be done before the deletions start.
