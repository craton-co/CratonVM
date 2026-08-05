# WP4.6 ConcurrentHashMap Boxed-Value Round-Trip Gap

## Status

Resolved. As of 2026-08-04 **every** probe in `wp4_6_chm_basic` runs in the
default suite — nothing in that file is `#[ignore]`d any more.

`WP4.6-FOLLOWUP-A` ("CHM `transfer()` data-loss after resize past 16 buckets")
closed with it, and it was never a `transfer()` defect:
`NativeContextImpl::java_strings_equal` answered a hard `false` for two Strings
whose character storage it could not decode, so a String-keyed
`ConcurrentHashMap.get` missed keys the map held whether or not a resize had
run. The Integer-keyed `test_chm_resize_path`, ignored under the same label,
passed the moment it was un-ignored. See
[`chm-get-misses-stored-key-in-process-RETIRED-20260804.md`](../chm-get-misses-stored-key-in-process-RETIRED-20260804.md).

## Repro

```powershell
cargo test -p cratonvm-vm --test wp4_6_chm_basic -- --nocapture --test-threads=1
```

The older extended interpreter corpus shows the same family when explicitly
enabled:

```powershell
$env:CRATONVM_RUN_EXTENDED_INTERPRETER_TESTS='1'
cargo test -p cratonvm-vm --test interpreter_tests concurrent_hashmap -- --nocapture --test-threads=1
```

## Original Observed Behavior

- `test_chm_pre_resize_put_get` returns `Ok(Some(Int(0)))` instead of
  `Ok(Some(Int(1)))`.
- `test_chm_mutation_cycle` returns `Ok(Some(Int(0)))` instead of
  `Ok(Some(Int(1)))`.
- The resize-heavy CHM tests are already ignored under
  `WP4.6-FOLLOWUP-A`.
- `test_chm_clear_empty` still passes and remains active.

## Root Cause

The committed fallback `ChmBasicProbe.class` hid the real failure by catching
the VM exception and returning `0`. After the WP4.6 harness was changed to load
fresh `build.rs`-compiled classes from `CRATONVM_TEST_CLASSES_DIR` first, the
underlying exception was visible:

```text
java/lang/NoSuchMethodError: java/lang/Integer.valueOf(I)Ljava/lang/Integer;
```

The exact wrapper `valueOf` native already existed in
`../../../native-builtins/src/lang_math.rs`, but default real-JDK/native test mode only
registered `register_essential_natives`. That path registered wrapper `TYPE`
clinits and generic `Number.intValue`, but not the exact wrapper autoboxing
surface emitted by modern `javac`.

After that registration gap was fixed, the probes also exposed an in-process
test isolation bug: the native `Integer`/`Long`/`Boolean.valueOf` caches stored
heap `ObjectRef`s in process-global arrays. A Rust test process can create more
than one independent `Vm`, so cached wrapper objects from one heap were reused
by another heap and later surfaced as stale/wrong-class boxed values.

## Fix

`register_essential_natives` now calls `lang_math::register_wrapper_natives`
after the `Math`/`StrictMath` registrations, so default mode has the standard
primitive wrapper `valueOf`/primitive-value methods available before fixture
and real-JDK bytecode autoboxing call sites resolve.

Wrapper class-id and boxed-value caches are now scoped by a monotonic
`SharedVm::vm_identity`. GC root scanning and post-compaction cache remapping
use the same VM identity, so cache entries stay canonical within one VM while
remaining isolated from other VMs in the same Rust process.

`CRATONVM_DISABLE_JIT=1` does not change the result, so this is not the old
W2-CHM `Integer.valueOf` JIT miscompile.
