# CRC32 / CRC32C Field-Layout Contract

Audience: the next-wave JIT intrinsic agent that will inline
`java.util.zip.CRC32.update*` and `java.util.zip.CRC32C.update*`. This document
pins down (a) where the running CRC value lives on a receiver object and (b)
whether a JIT intrinsic can statically know that offset.

## TL;DR

- `java.util.zip.CRC32` and `java.util.zip.CRC32C` each declare **exactly one
  instance field**: `private int crc`.
- That field lives at **instance field slot 0** (`first_field_index + 0`).
- The slot holds the **running (uncomplemented)** CRC state. The public 32-bit
  checksum returned by `getValue()` is `~crc & 0xFFFFFFFF`.
- A JIT intrinsic **can** statically know the slot is 0 — see "Can the JIT
  statically know the offset?" below for the exact justification and the one
  guard it must apply.

## Class shapes (JDK 25, verified by `javap -p`)

```
public class       java.util.zip.CRC32  implements Checksum {
  private int crc;                         // <-- the only instance field
  static final boolean $assertionsDisabled;// static, no instance slot
  ...
}
public final class java.util.zip.CRC32C implements Checksum {
  private int crc;                         // <-- the only instance field
  static final boolean $assertionsDisabled;// static
  private static final int   CRC32C_POLY, REVERSED_CRC32C_POLY;  // static
  private static final int[] byteTable, byteTable0..7;           // static
  private static final int[][] byteTables;                       // static
  private static final jdk.internal.misc.Unsafe UNSAFE;          // static
  ...
}
```

`java.lang.Object` contributes **zero** instance fields, and `Checksum` is an
interface (no fields). Therefore on both classes `crc` is the first and only
instance field: **slot 0**.

## Why slot 0 (this VM specifically)

- Neither `java/util/zip/CRC32` nor `java/util/zip/CRC32C` has an entry in
  `classloading::class_manager::synthetic_stub_fields(name)` — confirmed by
  grep. Both therefore load their field layout **from the real JDK 25 class
  file**, not from a synthetic stub.
- The real class file declares the single `int crc`. The class manager assigns
  it `first_field_index + 0`.
- Object-instance field padding (`stub_instance_count`) does not apply: there is
  no stub layout to pad to, so no extra leading slots are inserted.

## Running value vs. public value

The slot stores the **running** state, NOT the externally visible checksum:

| Operation         | Effect on slot 0 (`int crc`)                         |
|-------------------|------------------------------------------------------|
| `<init>` / `reset`| `crc = 0xFFFFFFFF` (`-1`)                            |
| `update(...)`     | `crc = step(crc, bytes)` (reflected CRC, in place)   |
| `getValue()`      | returns `~crc & 0xFFFFFFFF` — does **not** mutate    |

- **CRC32**: reflected polynomial `0xEDB88320` (IEEE 802.3). The pure-Java
  `CRC32` delegates its hot loop to the `updateBytes0(int,[B,II)I` *native*,
  passing the running `crc` and storing the native's return back into slot 0.
  Native impl: `native-builtins/src/zip_real.rs::crc32_step`.
- **CRC32C**: reflected polynomial `0x82F63B78` (= `Integer.reverse(0x1EDC6F41)`,
  the Castagnoli poly). In stock JDK 25 the hot loop is *pure Java* (no native)
  using `Unsafe`. In this VM the public `update`/`getValue`/`reset` methods are
  overridden by Rust natives. Native impl:
  `native-builtins/src/zip_crc32c.rs::crc32c_step`.

The constant `CRC_FIELD_SLOT = 0` is exported from `zip_crc32c.rs` so intrinsic
code can reference it by name instead of hardcoding a literal.

## Can the JIT statically know the offset?

**Yes — slot 0, statically.** Justification:

1. `CRC32` is a concrete non-final class but `crc` is `private`; no subclass can
   move or shadow it. `CRC32C` is `final`. So the field's slot is fixed for any
   receiver whose dynamic type is exactly `CRC32` / `CRC32C`.
2. Both classes load from the real JDK class file with no synthetic-stub
   padding, so `first_field_index` is stable and `crc` is at offset 0 within the
   instance-field region.
3. A JIT intrinsic for `update` is a *virtual* call site. It MUST guard on the
   receiver's exact class id (`CRC32` resp. `CRC32C`) — the same receiver
   class-id guard every virtual intrinsic already uses (see
   `docs/internal/intrinsic_table_contract.md` §3.4 and the `VirtualNative`
   cache arm). With that guard satisfied, reading/writing instance slot 0 as an
   `int` is correct and needs no field-resolution lookup.
4. The intrinsic must read slot 0 as a 32-bit `int`, treat it as the **running**
   value, fold bytes with the appropriate reflected polynomial
   (`0xEDB88320` for CRC32, `0x82F63B78` for CRC32C), and write the result back
   to slot 0. It must NOT complement on the way in/out — only `getValue()`
   complements, and `getValue()` is a separate (trivial) intrinsic candidate
   (`return ~slot0 & 0xFFFFFFFF`).

### Caveat for the intrinsic author

- The slot index is `first_field_index + 0`. If the JIT works with absolute
  instance-slot indices it should obtain `first_field_index` for the guarded
  class id from the class metadata (it is 0 in practice today, because `Object`
  has no instance fields, but resolving it is robust against future layout
  changes). The *relative* offset within the class's own field block is the
  invariant: **0**.
- If a future change ever adds a `synthetic_stub_fields` entry for either class,
  this contract must be revisited. Today there is none.

## Differential oracle

`native-builtins/src/zip_crc32c.rs` is the bit-exact CRC-32C reference. Its unit
tests assert the canonical vectors (CRC-32C of `"123456789"` = `0xE3069283`,
32 zero bytes = `0x8A9136AA`, etc.). A JIT CRC32C intrinsic can be
differential-tested against running the same Java program with the intrinsic
disabled (the native override path) — both must produce identical `getValue()`
results.
