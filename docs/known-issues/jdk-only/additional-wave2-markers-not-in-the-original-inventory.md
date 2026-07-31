# Additional `JDK-ONLY-WAVE2` findings not in the original wave-2 inventory

**Status:** OPEN — JDK-only wave-2 work items, filed 2026-07-31. These were
found while verifying the eleven inventoried findings. They are recorded here so
wave 2 does not rediscover them, and because two of them (§1 and §4) are the
same defect shape as items that *were* inventoried, in code nobody was looking
at.

Each item states where the marker lives so it can be re-read in full.

---

## 1. The JIT has the same missing-`NativeKind` hole as the interpreter's invoke cache — twice

`jit/src/lib.rs` ~4773 (`JitMICSlot`) and ~5092 (the polymorphic/megamorphic
variant):

> **JDK-ONLY-WAVE2 — this cache stores an entry pointer and no `NativeKind`.**
> `cached_entry_ptr` is a raw address that generated code `CALL R11`s on a
> class-id guard hit. …

and

> **same missing-`NativeKind` shape as `JitMICSlot`.** `entry_ptrs[i]` and
> `mega_entry_ptrs[i]` are raw addresses with no kind beside them, so a hit
> cannot re-check policy.

This is exactly
[cached invoke targets drop the `NativeKind`](cached-invoke-targets-drop-the-nativekind.md),
one layer down and worse: on the interpreter side a hit at least re-derives the
verdict for allow-listed classes; here generated machine code jumps to a raw
address with no opportunity to check anything. Fix the two together — a
`NativeKind` stored in `CachedInvokeTarget` that is dropped again when the JIT
installs an MIC slot buys nothing.

## 2. The JIT's compatibility policy is a process global, and so are the direct-helper addresses

`jit/src/lib.rs` ~4018:

```rust
static JIT_COMPATIBILITY_MODE: std::sync::atomic::AtomicU8 = …;
```

with the marker at ~4027:

> this is process-global where §2 wants per-VM state. It is latched toward
> strict so it cannot mis-execute …, but a `Compatible` VM sharing a process
> with a `JdkOnly` one **silently loses the thin direct-call helpers**. The
> wave-2 replacement is to move the `*_DIRECT_FN` helper addresses AND this
> policy into a single per-VM struct.

The module comment above it is honest about why the monotone latch was chosen:
the `*_DIRECT_FN` gates (`INTEGER_VALUE_OF_DIRECT_FN` et al.) are *already*
process-global `AtomicUsize`s written by `build_helpers`, so a per-VM policy
could not gate them coherently anyway. Making both VM-scoped together is the
real fix. Contract §2 forbids process globals for this feature's state precisely
because of the repo's history of process-global native caches leaking across
VMs.

## 3. Five JIT-reachable native-dispatch paths bypass `resolve_dispatch` entirely

`jit/src/lib.rs` ~4178 carries a `JDK-ONLY-NOTE` enumerating dispatch sites the
JIT crate cannot fix because they live in `vm/src/jit/helpers.rs`. Condensed:

1. `jit_invoke_dispatch` and `jit_invoke_virtual_mic` — **THE** JIT dispatch
   path for every compiled invoke that is not inlined, intrinsified or directly
   bound. Neither consults `resolve_dispatch` nor calls `record_invocation`.
2. JIT-only native fast paths that resolve natives by name on their own: the
   `java/lang/ClassLoader` `getResource*`-with-null intercept (a bare
   `natives.native_methods.find(..)` + `safe_native_call`),
   `hashmap_native_callback`, `matcher_native_callback`,
   `stringbuilder_native_callback`. *"Each returns a raw `NativeCallback` with
   no `NativeKind`, so none of them can honour §1 rule 3 or rule 4, and none is
   counted."*
3. `build_helpers` must call `set_jit_execution_policy` before the first
   compilation, and should skip the `set_*_direct_fn` registrations entirely
   under `JdkOnly`.
4. `cp_elidable_init_resolver` — eliding an `<init>` that is shadowed by a
   registered native skips the native (the known
   `jit-elidable-ctor-must-check-native-shadow` defect).
5. `INDY_STRING_CONCAT_FN` — assessed and deliberately **not** gated; the
   interpreter reaches the same bridge for the same sites.

Contract §11 requires *"every strict-mode native dispatch"* to be an
`ACC_NATIVE` bridge, a reviewed VM service or a proven intrinsic, and §4's
counters to be complete. Items 1 and 2 make both claims unverifiable for any
JIT-compiled frame.

## 4. Seven JIT "thin direct call" ladders bake a native reimplementation into emitted code

`jit/src/lib.rs` ~9994 and the six sites that reference it
(`STRING_LATIN1_LOWER_DIRECT_FN`, `INTEGER_VALUE_OF_DIRECT_FN`,
`INTEGER_INT_VALUE_DIRECT_FN`, `CONCURRENT_HASHMAP_GET_DIRECT_FN`,
`STRING_LOCALE_LOWER_DIRECT_FN`, `HASHMAP_PUT_DIRECT_FN`, and the `Map.get`
interface form):

> hard-coded (class, method, descriptor) exception list. Seven of these ladders
> bake a thin VM-side reimplementation of a registered native straight into the
> emitted CALL, bypassing `vm_exec::resolve_dispatch` entirely. Wave 1 gates them
> on policy via `direct_native_helper`; wave 2 should replace the name matching
> with a resolver callback that asks `resolve_dispatch` whether this triple is
> an approved `NativeKind::Intrinsic`, and delete the literals. **Do NOT delete
> the list before that resolver exists — every entry here is a measured hot
> path.**

Two of the seven are `java/lang/String` methods (`StringLatin1.toLowerCase`,
`String.toLowerCase(Locale)`), which makes the JIT a **third** location for the
forced-native `String` policy documented in
[the forced-native `String` policy exists twice](forced-native-string-policy-two-lists-that-disagree.md).
When that item says "two places that disagree", read it as *at least* three.

Wave 1 did gate these on policy (`direct_native_helper` refuses to bind under
`JdkOnly` and records the refusal), so this is the best-behaved of the four JIT
items — but the literals remain.

## 5. `jit-api`'s `force_native_cache` memoizes a ~1,400-line hard-coded dispatcher

`jit-api/src/lib.rs` ~204:

> this cell memoizes the *answer* of a ~1400-line hard-coded
> class-name/method-name dispatcher (`force_native_over_real_jdk_bytecode`)
> whose whole purpose is to make a registered native win over concrete real-JDK
> bytecode — the exact inversion §1 rule 4 forbids under `JdkOnly`. The list is
> a wave-2 removal and must NOT be deleted this wave.

The memo is an `Arc`-shared per-call-site cell that turns *"O(~55 string
comparisons) on every cached dispatch"* into an O(1) read. It is a perfectly
good optimisation of a list that should not exist; when the list goes, so does
the cell.

## 6. `JNI_NATIVE_METHODS` is a process global, and JNI dispatches are uncounted

`vm/src/native/jni.rs` ~4650:

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

`vm/src/native/jni.rs` ~1268, flagged as *"pre-existing, orthogonal to this
feature, not fixed here"*:

> the JNI `Call*Method` helpers silently swallow
> `MethodCallFailed::ExceptionThrown`, so a Java exception thrown by a
> JNI-initiated call never becomes a pending JNI exception. Fixing it would
> alter `Compatible` behaviour, which this wave may not do.

Wave 1 added `jni_surface_jdk_only` to intercept exactly one value —
`VmError::JdkOnly` — so a refusal behind a `CallObjectMethod` becomes a pending
JNI exception instead of vanishing. Everything else still takes the old path.
This is a real, separate JNI-correctness bug that happens to have been noticed
here.

## 8. The interpreter substitutes a *different class's* native for unresolvable interface calls

`vm/src/runtime/interpreter.rs` ~6241:

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
> legal. … NOT deleted this wave — removing shim mappings has regressed real-JDK
> boot before.

This belongs in the "silent wrong behaviour" tier alongside the layout item —
the VM runs `HashMap$KeyItr`'s native against a receiver that is not one.

## 9. The three `redefine_immune_*` predicates take a §1.4 decision outside `resolve_dispatch`

`vm/src/runtime/interpreter.rs` ~5869:

> the three `redefine_immune_*` predicates are hard-coded class/method-name
> exception lists … They encode "this native keeps winning even over
> **instrumented** bytecode", which is a §1.4 shadow decision taken outside
> `resolve_dispatch`. What should replace them: `NativeKind` — exactly
> `Intrinsic` should be redefine-immune, and everything else should yield to
> redefined bytecode, with no name list at all. NOT deleted this wave; the lists
> gate real Mockito/ByteBuddy behaviour.

## 10. The wave-1 name-only dispatch adapter hard-codes `compat_native_wins = true`

`vm/src/runtime/interpreter.rs` ~5633, inside the adapter that funnels the
name-only `find` call sites through `resolve_native_dispatch_wave1`:

> hard-coded `true` reproduces the pre-§7 "a registered native unconditionally
> wins here" of the `find` calls this replaces. Wave 2 replaces it with the real
> per-site compatibility verdict once `force_native_over_real_jdk_bytecode` and
> the forced-native `String` list … are unified.

Worth flagging because it is the one place where wave 1 deliberately *encoded*
the old behaviour as a constant. It will read as a bug to anyone who finds it
without the marker.

## 11. `check_override`'s exception chain is ~2,600 lines long

`vm/src/vm/vm_exec.rs`, in `check_override`, wave-1 marker text:

> everything after `method.is_abstract()` in the chain below is a hard-coded
> class/method exception list — ~2,600 lines of "this concrete JDK bytecode must
> lose to our registered native". Under §7 that is precisely the set of natives
> that are `NativeKind::SyntheticStub` or compatibility `Bridge`s mis-tagged as
> authoritative. Wave 2 must reclassify each entry in `native-builtins` and
> delete the list: `method.is_abstract()` (no `Code` attribute, so the native is
> the only implementation) is the **ONLY** clause §7 sanctions, and it survives
> as `resolve_dispatch`'s step-3b branch.

This is the largest single wave-2 deletion in the tree and the one most directly
blocked by
[`NativeKind` is ambient](native-kind-is-ambient-and-defaults-to-syntheticstub.md):
reclassifying ~2,600 lines' worth of entries is only safe once each native's kind
is a stated fact rather than an inherited one.

## 12. Cross-file documentation gaps flagged from code

* `libcratonvm/src/lib.rs` ~1484 — `docs/EMBEDDING.md`'s flat-C-API table and
  Rust-facade re-export list need rows for `cratonvm_create_with_compatibility`,
  `cratonvm_compatibility_mode`, `cratonvm_compatibility_mode_supported`, the
  `CRATONVM_COMPATIBILITY_*` constants and the `CompatibilityMode` /
  `ExecutionPolicy` / `JdkMode` re-exports. (`docs/EMBEDDING.md` has since been
  edited; re-check before acting.)
* `types/src/flag_groups.rs` ~839 — `vm-cli` should call
  `flag_groups::process_env_supersessions()` and print `Superseded::note()` once
  per entry next to the existing `legacy_direct` deprecation line.
* `types/src/error.rs` ~264 — `JdkOnlyViolation::render`'s signature is fixed at
  `(jdk_feature, verbose)`, and only `MissingBootClass::searched_image` carries a
  runtime image path, so the `java.home` line reads `<not recorded>` for the
  other six variants. Threading it in from `VmConfig` is a signature change.

## 13. Stale documentation paths in load-bearing code comments

Several dispatch sites cite known-issue docs that have since been fixed and moved
to `docs/internal/fixed-suite-bugs/` with a `-FIXED` suffix. The cited paths no
longer resolve:

| Cited in code as | Actually at |
|---|---|
| `docs/known-issues/stringjoiner-synthetic-native-real-jdk-field-mismatch.md` | `docs/internal/fixed-suite-bugs/stringjoiner-synthetic-native-real-jdk-field-mismatch-FIXED.md` |
| `docs/known-issues/threadpoolexecutor-execute-npe-on-ctl-regression.md` | `docs/internal/fixed-suite-bugs/threadpoolexecutor-execute-npe-on-ctl-regression-FIXED.md` |
| `docs/known-issues/threadpoolexecutor-execute-dispatch-degrades-to-synchronous.md` | `docs/internal/fixed-suite-bugs/threadpoolexecutor-execute-dispatch-degrades-to-synchronous-FIXED.md` |

These comments are the *only* explanation for why several of the duplicated
dispatch sites in this directory exist. A wave-2 engineer who follows the link
and finds nothing is likely to conclude the workaround is obsolete. Repointing
them is a one-line-per-site fix and should be done before the deletions start.
