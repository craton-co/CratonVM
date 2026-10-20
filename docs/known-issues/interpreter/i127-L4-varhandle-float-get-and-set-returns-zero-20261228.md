# i127-L4: float / double atomics through `Unsafe` lose the value (`getAndSet` returns 0.0)

Interpreter round i6, wave 127, lane L4. Found by the orchestrator's run of w124 (869ab6e395).

## Status

FIXED in code, unbuilt (wave 127, lane L4). There is no switch, because this is a plain correctness
fix confined to the int-/long-width `Unsafe` natives when they meet a `float` / `double` slot.
Move this page to `docs/internal/` once the orchestrator's build passes the checks under
"Verification".

## Symptom

`tools/probes/interp/L4/L4W123RealVarForm.java` DIFFs against HotSpot 25 in `--jdk-only`, with the
JIT and with `--nojit`, both by default and with `CRATONVM_VH_REAL_VARFORM=1`:

```
< tmh f GET_AND_SET (Box,float)float -> Float:2.5      > ... -> Float:0.0
< tmh f GET_AND_ADD (Box,float)float -> Float:4.0      > ... -> Float:0.0
< tmh f GET (Box)float -> Float:6.5                    > ... -> Float:2.5   (and the invoker rows)
< tmh d GET_AND_SET (Box,double)double -> Double:3.5   > ... -> Double:7.0
```

The probe's sequence is SET v1, CAS(v1,v2), CAE(v2,v1), GAS(v2), WCAS_PLAIN(v1,v1), GAA(v1). Our
answers mean the following:

- `compareAndExchange` on a float or double field returned the right witness, but stored nothing.
  For double, the field stayed at 7.0, so the following GAS returned 7.0.
- `getAndSet` / `getAndAdd` on a float field read the old value as 0.0. GAS(4.0) returned 0.0 and
  stored 4.0. GAA(2.5) also read 0.0, so it returned 0.0 and stored 0.0 + 2.5 = 2.5. HotSpot ends
  at 4.0 + 2.5 = 6.5.

## Cause

In `--jdk-only`, the JDK's `VarHandleFloats` / `VarHandleDoubles` bodies run. Their atomics are
`Unsafe` int / long atomics over the raw bits:

- `compareAndExchangeFloat(o, off, e, x)` is `compareAndExchangeInt(o, off, floatToRawIntBits(e),
  floatToRawIntBits(x))`.
- `getAndSetFloat` / `getAndAddFloat` loop over `intBitsToFloat(getIntVolatile(o, off))` and
  `weakCompareAndSetInt(o, off, bits, newBits)`.
- The double atomics are the same with `long`.

A `float` / `double` slot in CratonVM reads back as `Value::Float` / `Value::Double`. Two of the
natives did not handle that:

1. **`compareAndExchangeInt` / `Long`.** The winning registration is
   `unsafe_natives::cae_int_impl` / `cae_long_impl`. Through `unsafe_compare_exchange_heap_slot`,
   these compared the slot with `unsafe_cas_values_equal`, which had no arm for Float-vs-Int or
   Double-vs-Long, so the exchange never matched. The witness mapping then turned the non-`Int`
   `old` into `expected`, so the caller saw a successful exchange although nothing had been stored.
   The VM's own CAS equality (`vm_exec::values_equal_for_cas`) already compares cross-tag
   primitives by bits, which is why `compareAndSetFloat` (a plain `compareAndSetInt`) worked.
2. **`getIntVolatile`.** `native_unsafe_get_int_volatile_arena` -> `native_unsafe_get_int_volatile`
   handed the slot's `Value::Float` to the `I` return, which `intBitsToFloat` read as 0.0. The CAS
   that followed still landed, because the VM CAS compares by bits. `getInt` had been fixed for the
   same shape in JIT round 20 wave 7. `getIntVolatile` had not, because `getFloatVolatile` shares the
   unwrapped native.

In `--compatible` mode the VM's own `VarHandle` natives (`lang_invoke.rs`, `vh_cas_rmw` /
`vh_compare_and_exchange_slot`) work on `Value::Float` directly and are not affected. They are
checked by the same probe.

## Fix

All of it is in `native-builtins/src/unsafe_natives_ext.rs`, `unsafe_natives.rs`, and `lib.rs`
(`synthetic_cas`):

- `unsafe_cas_values_equal` compares Float-vs-Int and Double-vs-Long by raw bits, as HotSpot's
  `cmpxchg` does: -0.0 never matches 0.0, and a NaN matches its own bits.
- `unsafe_compare_exchange_heap_slot`: once such a slot matches by bits, the CAS expects the
  `Value::Float` / `Value::Double` it just read and stores the update re-tagged
  (`unsafe_retag_bits_like`). A context that compares tags strictly therefore still lands it.
- The witnesses of `cae_int_impl` / `cae_long_impl` and of
  `native_unsafe_compare_and_exchange_int_element` / `_long` are a float / double slot's bits
  (`unsafe_int_bits_of` / `unsafe_long_bits_of`), not `expected`.
- `cae_int_impl` / `cae_long_impl` gained the raw-bytes arm (`unsafe_wide_raw_cae`) that
  `native_unsafe_compare_and_exchange_int` already had, for `compareAndExchangeFloat` /
  `Double` on a `float[]` / `double[]` element.
- `native_unsafe_get_int_volatile_arena` / `_long_` (the `getIntVolatile` / `getLongVolatile`
  registrations) answer a float / double slot's bits. The unwrapped natives now pass a float /
  double static (null base, or a mirror base through `synthetic_get`) through instead of reading
  it as 0. `getFloatVolatile` / `getDoubleVolatile` share them and want the floating value.
- A static's CAS (`unsafe_static_cas`, the null-base `compareAndSetLong`, `synthetic_cas`) keeps a
  float / double static's tag when it stores bits.

## Not covered

`getIntAcquire` / `getIntOpaque` / `getLongAcquire` / `getLongOpaque` are still registered to the
unwrapped natives. An int-width acquire / opaque read of a float slot answers the `Value::Float`.
No JDK float atomic reads that way: `getFloatAcquire` is `getFloatVolatile`.

## Verification

- Probe `tools/probes/interp/L4/L4W127FloatAtomics.java` exercises getAndSet / getAndAdd /
  compareAndExchange (and the Acquire / Release forms) on float and double. It covers field,
  static, and array-element VarHandles; `toMethodHandle` with `invokeExact` and
  `invokeWithArguments`; `sun.misc.Unsafe` bits atomics; `jdk.internal.misc.Unsafe`'s
  `getAndSetFloat` / `getAndAddFloat` / `compareAndExchangeFloat` and the double twins by
  reflection; the -0.0 / NaN raw-bit rows; and 20,000-iteration hot loops. Run it with
  `--add-exports java.base/jdk.internal.misc=ALL-UNNAMED` in both modes, with the JIT and with
  `--nojit`. It must match its HOTSPOT_EXPECTED block, which is identical under default flags and
  `-Xint`.
- `tools/probes/interp/L4/L4W123RealVarForm.java` must match in both modes, with the JIT and with
  `--nojit`, both by default and with `CRATONVM_VH_REAL_VARFORM=1`.
- Unit tests: `unsafe_natives::tests::i127_cae_int_and_long_over_float_and_double_fields`,
  `unsafe_natives_ext::i127_float_bits_cas_tests`.
