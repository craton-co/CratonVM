# Code Review: `native-builtins` java.lang/util layer (module `nb-lang`)

Reviewer: Fable (Opus 4.8) — 2026-06-10
Scope: `native-builtins/src/{lang_*,util_*,classloader,serialization,unsafe_natives,atomic_updater,panama,vector_api,jdk25_concurrency,properties_sidetable,shared_secrets_bridge,deprecated_*,tests_extracted}.rs` (~81k LOC measured by `wc -l`).

Static review only. No cargo invoked, no files modified.

---

## Summary

This is a large, mature layer that has clearly been through multiple security-audit rounds (the off-heap `Unsafe` accessors and the Panama FFI memory paths carry "audit-round4/5/6" and "SECURITY FIX (V5)" annotations and are genuinely well-defended: per-byte arena bounds checks, size caps, overflow-checked address arithmetic, and IAE-on-stale-cache). The raw-pointer surface (panama.rs, unsafe_natives.rs) is in good shape.

The most important findings are in **serialization.rs**: the *synthetic* `ObjectInputStream.readObject` native fast-path decodes attacker-controlled class names and runs `<clinit>` on them **without consulting the JEP-290 class-name filter** (the filter is only wired into the real-JDK `resolveClass`/`resolveProxyClass` natives, which this fast path bypasses), and several length fields from the wire drive unbounded allocations when no `maxbytes`/`maxarray` filter is installed. There is also an always-on synthetic shim in **classloader.rs** (`cglib_guard_value`) that fakes cglib proxy classes as `java/lang/Object` to dodge a real `defineClass` SEGV — a direct violation of the project's "no synthetic stubs" policy.

Test coverage across the module is high in raw count (~1075 `#[test]` fns) and well-targeted on the risky raw-memory and serialization paths; the gaps are the MethodHandle/VarHandle machinery (lang_invoke.rs), reflection (lang_reflect.rs), and StackWalker.

Estimated coverage: **~70%**. Does not plausibly reach 85% module-wide because three large files (lang_invoke 5347 LOC / 10 inline tests, lang_reflect 1591 / 1, lang_stackwalker 890 / 1) are thinly tested relative to size.

---

## Bugs

### B1. (low) `Unsafe.setMemory` on-heap non-array path can write past object field count
`unsafe_natives.rs:694-697` — for a non-array heap target the fill loops `for i in 0..bytes { ctx.set_field(obj_ref, off + i, fill_value) }`. `bytes` and `off` come from the (trusted) Unsafe caller but there is no check that `off + i` stays within the object's declared field count; whether this is contained depends entirely on `NativeContext::set_field`'s own bounds handling. The array path (line 686) *is* bounds-checked via `unsafe_array_write_bytes`. Recommend mirroring that guard for the field path. Trust caveat: `Unsafe` is inherently a trusted API, so severity is low.

### B2. (low) `Date.getTimezoneOffset()` always returns 0
`deprecated_util.rs:265-271` — hardcoded `0` ("UTC assumption"). The deprecated `java.util.Date.getTimezoneOffset()` is specified to return the offset of the local time zone. Apps that still call it (legacy date formatting) get wrong values. Low priority (deprecated API).

### B3. (low) `jlia_link_method_handle_constant` returns null
`shared_secrets_bridge.rs:699-708` — `JavaLangInvokeAccess.linkMethodHandleConstant` (the `ldc CONSTANT_MethodHandle` path) returns `Object(None)`. If real bytecode reaches this via SharedSecrets rather than the bytecode fallback, the null surfaces as a downstream NPE far from the cause. Deferred-incomplete rather than wrong-success; see also Stubs.

---

## Vulnerabilities

### V1. (high) JEP-290 class-name filter bypassed by the synthetic `readObject` deserializer
`serialization.rs:1411` (native `readObject`) → `ois_read_value` (1127) → `ois_read_object` (1209) → `ctx.ensure_class_initialized(&desc.class_name)` (1214).
The synthetic deserializer enforces only the *resource-limit* dimensions of the filter (depth/refs/bytes/array via `filter_*`). It never calls `evaluate_serial_filters(addr, class_name)`. The class-name allow/deny pattern filter (`SerialFilter::check`, fully implemented and tested) is consulted **only** on the real-JDK `resolveClass`/`resolveProxyClass` natives (1631, 1667), which this fast path does not go through. Consequence: when the synthetic `ObjectInputStream` native runs, an attacker-named class in the stream is resolved and `<clinit>`-run before any deny-list/allow-list filter applies — the classic gadget-chain entry point JEP-290 exists to block. Fix: call `evaluate_serial_filters` against `desc.class_name` (and each array/proxy element class) inside `ois_read_object`/`ois_read_array` before `ensure_class_initialized`/allocation.

### V2. (medium) Unbounded allocation from `TC_LONGSTRING` / `TC_ARRAY` length when no filter installed
`serialization.rs:1172-1182` (`TC_LONGSTRING`): reads an 8-byte `u64` length straight off the wire, casts to `usize`, and calls `ois_buf_read(addr, len)` which does `vec![0u8; n]` (line 433) on a short read — i.e. it allocates the *claimed* length even when the buffer is empty. `filter_account_bytes` runs first but returns `true` (unbounded) when no `maxbytes` filter is set (line 199). `ois_read_array` (1280-1292) similarly allocates `new_array(elem_type, length)` for `length` up to `i32::MAX` when no `maxarray` filter is installed (`filter_check_array` returns `true` on the no-filter branch, 276). Real JDKs ship a built-in default array/depth limit; here a single malformed frame triggers a multi-GB allocation / OOM-abort DoS. Fix: apply a hard internal ceiling (or honour the JDK default filter) even when no app filter is installed.

### V3. (medium) `Unsafe.setMemory` field-write bounds — see B1
Cross-listed: an out-of-range `off+bytes` on a non-array heap object is not range-checked in this layer (`unsafe_natives.rs:694`). Memory safety depends on the `set_field` callee. Trusted-caller caveat keeps this medium/low.

---

## Stubs and Unimplemented

### S1. (high — policy violation) Always-on cglib synthetic shim fakes proxy classes
`classloader.rs:757-816` — `cglib_guard_value`/`cglib_placeholder_mirror` short-circuit `defineClass*` for any class whose name contains `$$EnhancerByCGLIB$$` or starts with `net/sf/cglib/proxy/`, returning a `java/lang/Object` mirror instead of actually defining the class. This is an *always-on* (no env gate) synthetic shim that fakes application behaviour (cglib proxies silently become `Object`) to mask a real `defineClass` SEGV. The project's "no synthetic stubs / fix the underlying VM bug" policy forbids exactly this. The right fix is to make the real `defineClass` path not SEGV on cglib-generated bytecode.

### S2. (medium) `linkMethodHandleConstant` returns null
`shared_secrets_bridge.rs:699-708` — deferred ("Owner C WP1.6"); returns null. Incomplete native.

### S3. (low) `java.lang.Compiler.compileClass/compileClasses/command`
`deprecated_lang.rs:328-349` — return `false`/null. This is actually *correct* for the removed JDK `Compiler` class (no-op by spec); listed for completeness only.

### S4. (low) `Date.getTimezoneOffset` hardcoded 0 — see B2.

### S5. (low) `deprecated_internal.rs:368` `NotImplemented` and `deprecated_verify.rs:171-179` "deprecated API not implemented" error returns
These return explicit `NotImplemented`/error rather than faking success — acceptable per policy (clear error, not a fake value), but they bound real functionality and are worth tracking.

(The `_ => unreachable!()` in `lang_invoke.rs:5242/5254/5285/5316` are all inside `#[test]` modules — not stubs. The `NotImplemented` returns in `lang_class.rs:2145/2159` are legitimate error paths in `Class.newInstance`, not stubs.)

---

## Performance

### P1. `String.hashCode` drains the value array via per-element virtual dispatch
`lang_string.rs:638-696` — for an uncached hash the code does N `ctx.get_array_element` trait-dispatch calls into a thread-local scratch buffer, then a second tight loop. The two-phase split is a deliberate optimization, but the dominant cost is still N virtual-dispatch element reads. If `NativeContext` exposed a bulk byte-slice accessor for String backing arrays, hashCode (and `charAt`-in-loops, `getChars`, equals) could read the whole array once. Worth a bulk-read API on the hot string path.

### P2. Serialization buffers keyed by `usize` address in global `Mutex<HashMap>`
`serialization.rs:33-66` — every OIS/OOS read/write takes a process-global mutex (`oos_buffers`, `ois_buffers`, `handle_registry`, `ois_handles`, `ois_filter_state`). Concurrent (de)serialization on multiple threads serializes on these locks. Per-stream state could live on the stream object's side table to avoid the global contention.

### P3. `ois_read_object` re-snapshots declared fields multiple times
`serialization.rs:1216-1253` — `ctx.declared_fields(class_id)` is called and filtered three separate times (default-init pass, names snapshot, and the count). For a class with many fields and many instances in a stream this is repeated O(fields) work per object. Snapshot once and reuse.

### P4. `set_memory`/`copy_memory` off-heap loop is byte-at-a-time through arena accessor
`unsafe_natives.rs:704-715` (and the off-heap copy loop) call `unsafe_arena_put_byte(offset+i, ...)` per byte. Correct and safe, but for large `setMemory`/`copyMemory` this is a per-byte validated call; a range-validate-once-then-bulk-write fast path would cut overhead substantially for the common big-buffer case.

---

## Tests

Inventory (inline `#[test]` counts): serialization 129, jdk25_concurrency 168, lang_class 127, tests_extracted 95 (VM-level integration), lang_string 63, vector_api 56, panama 52, lang_math 44, util_time 33, lang_system 28, deprecated_util 46, classloader 75, others smaller. Total ~1075 across the scope.

Adequacy by risk:
- **Well covered:** serialization filter parsing + reject flows, panama bounds/overflow, off-heap Unsafe accessors, VarHandle meta-resolution, string layout/coder handling, time/calendar arithmetic.
- **Thin relative to size/risk:** `lang_invoke.rs` (5347 LOC, 10 inline tests) — MethodHandle/MethodType resolution and signature-polymorphic edge cases; `lang_reflect.rs` (1591 LOC, 1 test); `lang_stackwalker.rs` (890 LOC, 1 test). Some of this is reached indirectly via tests_extracted's real-`Vm` tests, but the resolution edge cases are not directly exercised.
- **Missing for the findings above:** no test asserts that the synthetic `readObject` path enforces the *class-name* filter (V1); no test feeds an oversized `TC_LONGSTRING`/`TC_ARRAY` length with no filter installed (V2); no test for `Unsafe.setMemory` on a non-array object past its field count (B1).

Estimated coverage: **~70%**. Does not plausibly reach 85% module-wide because of the three large under-tested files.

Most important missing tests:
1. Deserialize a stream whose `TC_OBJECT`/`TC_ARRAY` class name is on a `!`-deny pattern with the synthetic `readObject` native → assert `InvalidClassException` (currently would pass through). (V1)
2. `TC_LONGSTRING` with `len = 2^40` and an empty buffer, no filter installed → assert a bounded error, not a giant allocation. (V2)
3. `ois_read_array` with `length = Integer.MAX_VALUE`, no `maxarray` → bounded rejection. (V2)
4. lang_invoke: `findVirtual`/`findStatic`/`asType` adaptation and `invokeExact` arity/type-mismatch error paths.
5. lang_reflect: `Method.invoke` boxing/unboxing for each primitive return descriptor and `Field.set` widening rules.

---

## Feature Suggestions

1. **Honour a default serial filter / built-in resource ceilings.** Even with no app-installed filter, apply JDK-style default `maxarray`/`maxdepth` and wire class-name filtering into the synthetic deserializer (fixes V1+V2 together).
2. **Bulk String-array accessor on `NativeContext`** for the hot string natives (hashCode/charAt-loops/getChars/equals) — removes per-element virtual dispatch (P1).
3. **Remove the cglib synthetic shim by fixing real `defineClass`** on generated proxy bytecode (S1) — eliminates a policy-forbidden fake.
4. **Per-stream serialization state side-table** instead of five process-global mutexes (P2).
5. **Range-validate-then-bulk fast paths** for large `Unsafe.setMemory`/`copyMemory` and `MemorySegment.fill`/`copy` (P4).
6. **Complete `linkMethodHandleConstant`** (S2) so `ldc CONSTANT_MethodHandle` doesn't depend on a bytecode fallback that may not exist for all call sites.

---

## Files sampled vs fully read

**Read in depth (riskiest regions):**
- `panama.rs` — fn-ptr transmute (107-127), `ofArray` copies (645-759), `copy`/`fill` bounds (761-918), `pe_segment_access_addr` + get/set (935-1086), downcall marshalling/CIF/ffi_call (1340-1530).
- `serialization.rs` — filter accounting (195-296), `parse_serial_filter` (312), buffer read (408-444), `ois_read_value`/`ois_read_object`/`ois_read_array` (1127-1342), `readObject`/`readUnshared` natives (1403-1492), `resolveClass`/`resolveProxyClass` filter gates (1606-1680), `SerialFilter` parse/check (2432-2530).
- `unsafe_natives.rs` — get/put-at-address accessors (305-424), `set_memory`/`copy_memory` consolidated (642-790), registration table (860-885).
- `classloader.rs` — cglib shim (748-816), GC-remap unsafe (86-89).
- `lang_string.rs` — `hash_code` (593-704), `char_at` (719-777); structure-scanned the rest.
- `shared_secrets_bridge.rs` — JLIA bridges (676-714).

**Sampled (structure grep + targeted reads):** `lang_class.rs` (NotImplemented sites), `lang_invoke.rs` (unreachable sites + test layout), `lang_math.rs` (arg-fallback scan), `util_time.rs` (unwrap audit — all infallible `write!`/test), `deprecated_lang.rs`/`deprecated_util.rs`/`deprecated_internal.rs`/`deprecated_verify.rs` (stub scan), `atomic_updater.rs` (unsafe = test-only), `jdk25_concurrency.rs` / `vector_api.rs` / `properties_sidetable.rs` (stub/pattern scan), `tests_extracted.rs` (coverage inventory).

**Not deep-read:** `lang_reflect.rs`, `lang_stackwalker.rs`, `lang_system.rs`, `lang_misc.rs`, full bodies of `lang_class.rs`/`lang_invoke.rs`/`util_time.rs`/`vector_api.rs`/`jdk25_concurrency.rs` (scanned, not line-by-line) — flagged as test gaps rather than reviewed for bugs.
