# An empty JNI native costs ~500 ns per call with the default flags (HotSpot: ~5-10 ns)

> **STATUS (2026-09-29, gce e1/x): KEEP -- no cost change measured.** Arm A `Gcd1JniCostProbe` rows (`*_jnicost_A_1..5`, `verify-e1/ve1`) print HotSpot's lines on all three collectors on e1; the orchestrator reports the default `noop` unchanged by e1 at about 0.8 us, but the follow-up e1b REGRESSED the default arm on G1 and ZGC. **Remaining:** that regression; the invoke-path half and item 4 (`SharedVm::get_arc` per `JniContextGuard::install`, `vm_exec.rs`); measure arm A medians against d10 after them.

> **STATUS (2026-09-29, gce e1/j): NARROWED -- item 1 (resolution by
> strings) is fixed in code and awaits the probe. The invoke-path half and
> item 4 stay OPEN (they are in `vm_exec.rs` / `invoke.rs`, and no JNI lane
> owns them).**
>
> *Landed, default ON, no flag (the results are identical by construction):*
>
> - `NativeRealm::jni_native_generation`
>   (`vm/src/vm/realms/native_realm.rs`, initialised in `vm/src/vm/vm_init.rs`
>   to `vm_identity << 32`) is the d10/j cross-lane request 1. It is bumped
>   after every write to the table (`jni::register_jni_native`,
>   `jni_unregister_natives`).
> - `jni::find_jni_native` serves a hit from a one-entry per-thread memo
>   (`JniFnMemo` in `NativeCallTls::fn_memo`). The memo is valid for the same
>   realm, generation and triple, compared by value. It replaces the FNV hash
>   of the triple, the `RwLock` read and the map probe with three string
>   compares and one load. A miss is never memoised.
> - With the package on (`CRATONVM_JNI_NATIVE_TRANSITIONS=1`),
>   `JniNativeCall::enter_engaged` takes one `Arc<SharedVm>` per call instead
>   of two.
> - The package's per-call cost, the reason it cannot be the default, is on
>   `gce-e1j-in-native-deposit-rescans-the-whole-stack-above-the-jit-chain-20260929.md`
>   (fixed in code).
>
> *Verify:*
>
> - `cargo test -j 5 -p cratonvm-vm --lib find_jni_native_memo_follows_the_registration_generation`.
> - `Gcd1JniCostProbe` arm A, 5 runs interleaved with the d10 binary, each
>   collector:
>   - stdout is unchanged (`noop: ok` ... `PASS all 4`);
>   - `noop` is expected a few tens of ns below d10's median. A measured
>     median at or above d10's is not a regression of this item; the
>     remaining ~0.8 us is the invoke-path half.

> **STATUS (2026-09-28, gcd d10/j, lane jni10; by reading, no cargo): NARROWED
> -- the allocation and descriptor items of the JNI files landed, default ON
> (no behaviour change, no flag); the resolution-by-strings item and the
> invoke-path half stay OPEN (other lanes' files, requests below).** d7/d8
> baseline: `Gcd1JniCostProbe` arm A `noop` 524-530 ns (`jnicost_A_1..5`).
>
> *Landed (`vm/src/native/jni.rs`):*
>
> * `dispatch_jni_native` reads the parameter tags in place
>   (`param_type_tags`, an iterator over the descriptor with exactly
>   `parse_param_types_inner`'s answer) instead of `parse_param_types_cached`
>   (a `String`-keyed `HashMap` probe in a `RefCell` plus a `to_vec()` per
>   call), and builds the C argument list in a `SmallVec<[JniArg; 16]>` on
>   the stack instead of a heap `Vec` (items 2 and 3 of "Where it goes");
> * the implicit local frame's storage is reused: `truncate_local_frames`
>   keeps the storage of a closed frame (emptied, at most
>   `SPARE_LOCAL_FRAME_MAX` = 64 slots, in `NativeCallTls::spare_local_frame`)
>   and `push_local_frame` takes it back -- no `malloc`/`free` per dispatch
>   (item 2).
>
> *Still open, with owners:*
>
> * item 1, resolution by strings (`find_jni_native`'s FNV + `RwLock` +
>   SipHash probe per call) -- a per-thread memo in `jni.rs` needs a
>   registration generation on `NativeRealm` (`vm/src/vm/realms/native_realm.rs`,
>   no JNI lane owns it): exact diff in `docs/internal/gc-defects-round-20260927/d10-j-report.md`
>   (cross-lane request 1);
> * the interpreter/JIT half before the door (`invoke.rs`'s `CacheMiss` for a
>   JNI native, then `invoke_on_class_shared_inner`'s class-manager reads,
>   `find_method_recursive`, `class_name.to_string()` and
>   `native_methods.find_with_kind` -- all string-keyed, all per call): the
>   frames lane; a per-method JNI dispatch memo is the shape (same report);
> * items 4 (`get_arc` per install) and 5 (the shared counter): unchanged.
>
> *Verify (orchestrator, each of `-XX:+UseGenerationalGC`, `-XX:+UseG1GC`,
> `-XX:+UseZGC`):* `Gcd1JniCostProbe` arm A, 5 runs interleaved with the
> d7 binary if available: stdout `noop: ok` ... `PASS all 4` unchanged; `noop`
> median expected a few tens of ns below 526 (the allocations and the cache
> probe removed; the invoke-path half dominates what is left). Unit tests:
> `cargo test -j 5 -p cratonvm-vm --lib param_type_tags_match_the_parser`,
> `cargo test -j 5 -p cratonvm-vm --lib a_closed_local_frame_is_reused_empty`,
> and the dispatch regressions `cargo test -j 5 -p cratonvm-vm --lib dispatch_`.
> Target unchanged: `noop` under 100 ns once item 1 and the invoke-path memo
> land.
>
> *Previous (d8/x, wave d7): OPEN, unchanged; `noop` 526, 526, 524, 527, 530 ns
> (`jnicost_A_1..5`), `array-length` 17 ns, `int-region` 24-27 ns.*

*Filed 2026-09-28 by gcd d5/f (lane native5), by reading; no build and no
profile in the lane. Severity: LOW-MEDIUM (performance only: a JNI-heavy
library -- netty's epoll/kqueue transports, netty-tcnative, lz4/zstd-jni, JNA's
`invoke` -- pays it per call; no wrong result).*

## Evidence

`tools/bench/Gcd1JniCostProbe.java` on the gcd d4 build (Linux, medians of 7
reps, 5 runs agree), default flags: `noop 505 ns` -- a Java loop calling
`static native int noop(int)`, whose C body is `return x + 1`. The JNIEnv
calls themselves are cheap on the same arm (`array-length 14 ns`,
`int-region 23 ns`), so the ~500 ns is the Java -> native -> Java dispatch,
not JNI's function table. HotSpot pays a few ns (a compiled native wrapper:
a state store, the call, a state store and a poll).

## Where it goes (by reading; to be confirmed with a profile)

The call reaches `vm/src/vm/vm_exec.rs`'s generic native dispatch (the
`is_native` door, ~36780) from the caller's invoke path, then the JNI arm
(gcd d5/f's region) and `native::jni::dispatch_jni_native`. Per call, in order:

1. **Resolution by strings, every call.** `find_jni_native` hashes class name,
   method name and descriptor (FNV over ~40 bytes) and takes the VM's
   `jni_native_methods` `RwLock` for a `HashMap` probe; the door before it
   checks the registry-native table and the `tomcat/jni` / `tcnative` prefixes
   (`starts_with` on the class name), and `check_native_dispatch_capability`
   runs only on the registry arm. Nothing caches the resolved `fn_ptr` on the
   method.
2. **Three heap allocations.** `JniImplicitFrameGuard::enter` ->
   `push_local_frame(16)` (a `Vec` with capacity 16, freed at
   `truncate_local_frames`); `parse_param_types_cached(descriptor)` returns a
   `to_vec()` copy of the cached tags (after a `RefCell` borrow and an LRU
   probe keyed by the descriptor string); `dispatch_jni_native` builds
   `jargs: Vec<JniArg>`.
3. **Descriptor parsing twice more.** `count_descriptor_params(descriptor)`
   for the arity check, and `descriptor.rfind(')')` for the return kind in
   both `dispatch_jni_native` and the door.
4. **Thread-local traffic.** `JniContextGuard::install_for_native`:
   `replace_jni_context` -> `SharedVm::get_arc` (a read of the `self_arc`
   `RwLock` and a `Weak::upgrade` CAS, then the matching `Arc` drop: atomic
   RMWs on lines every JNI thread of the VM shares) into `JNI_SHARED_VM` and
   back, `JNI_THREAD` swap,
   `JNI_CONTEXT_GENERATION` bump twice, `jni_native_class` swap;
   `raw_local_escapes`, `RawLocalsDispatch::enter`, `JniNativeCall::enter`
   (each one latched flag load when off); `native_callee_memo::enter_native`.
5. **A shared counter.** `jni_bridge_invocations.fetch_add` on the VM's
   `NativeRealm` -- a contended cache line with several JNI threads.
6. **The return.** `jni_pending_exception_after_native` (thread-local read,
   and the VM's pending-exception protocol), the result decode.

Plus the interpreter / JIT side before the door (the caller's invoke of a
native method: not this lane's, `vm/src/runtime/interpreter/invoke.rs` and
the JIT's native-call helper). A `perf record -g` of `Gcd1JniCostProbe`'s
`noop` case splits the ~500 ns between the two halves.

## Proposed fix

In the JNI region (gcd d5/f's this wave; a later JNI lane):

* cache the resolved `fn_ptr` (and the parsed parameter tags and return kind)
  per `(class, method)` -- on the method's native-callee memo, or a per-VM
  `ClassId + method index` keyed table -- invalidated by `RegisterNatives` /
  `UnregisterNatives`;
* keep the implicit frame's `Vec` (reuse the frame storage: truncate to the
  base, do not free) and build `jargs` in a stack array (`SmallVec<[JniArg; 8]>`
  or a fixed array for the <= 8-argument integer fast path, which is already
  special-cased in `call_jni_marshalled`);
* count `jni_bridge_invocations` per thread and fold at report time, or only
  under the census flag;
* hold a borrowed `&SharedVm` in `JNI_SHARED_VM` for the dispatch instead of
  an `Arc` clone (the dispatch's frame keeps the VM alive), keeping the `Arc`
  only for attach paths.

Performance changes on the default path: land them behind an opt-in flag first
and A/B with `Gcd1JniCostProbe` (`noop`) plus the JNI census workloads of
`gcd-d2i-jni-native-methods-are-counted-mutators-20260927.md` item 3.

## How to verify

`Gcd1JniCostProbe` default arm, `noop` median: today ~505 ns; target
< 100 ns after the caching and allocation items (the interpreter/JIT half
bounds the rest). stdout unchanged (`noop: ok` ... `PASS all 4`).
