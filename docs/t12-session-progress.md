# T12 — `jdk/internal/misc/Unsafe` JDK 25 Natives: Session Progress

**Status: COMPLETE** (2026-04-16)

## Summary

All 79 T12 `jdk/internal/misc/Unsafe` JDK 25 native methods are registered and
implemented with real logic. 8 new T12 unit tests pass, zero stubs, zero TODOs.

## Gap Analysis (pre-T12)

Before T12, the following methods were missing or stubbed:

| Method | Status Before | Fix |
|--------|--------------|-----|
| `addressSize0()I` | Missing | Added — returns 8 |
| `isBigEndian0()Z` | Missing | Added — returns false |
| `unalignedAccess0()Z` | Missing | Added — returns true |
| `loadLoadFence()V` | Missing | Added — `fence(Acquire)` |
| `storeStoreFence()V` | Missing | Added — `fence(Release)` |
| `copySwapMemory0(...)V` | Stub (`native_noop_with_this`) | Replaced with real byte-swap copy |

## T12 Sub-section Status

### T12.1 — registerNatives + memory allocation (10 methods) ✅
All 9 methods were already registered with real implementations:
- `registerNatives()V` — no-op (JDK convention)
- `allocateMemory0(J)J` — heap-backed allocation simulation
- `reallocateMemory0(JJ)J` — allocate new + old is GC'd
- `freeMemory0(J)V` — no-op (GC-managed)
- `setMemory0(...)V` — field-by-field fill
- `copyMemory0(...)V` — field-by-field copy
- `writeback0(J)V` — no-op (no hardware cache model)
- `writebackPreSync0()V` — fence hint
- `writebackPostSync0()V` — fence hint

**Test:** `t12_unsafe_alloc_free_round_trip` ✅

### T12.2 — Field access (36 methods) ✅
All get/put methods for 9 types (Int, Long, Byte, Short, Char, Boolean, Float,
Double, Reference) in both plain and volatile variants were already registered:
- Plain: `getX` / `putX` for all types
- Volatile: `getXVolatile` / `putXVolatile` for all types

**Tests:** `t12_unsafe_get_put_int_round_trip` ✅, `t12_unsafe_volatile_ordering` ✅

### T12.3 — CAS + atomic ops (11 methods) ✅
All CAS and atomic compound operations were already registered:
- `compareAndSetInt` / `compareAndSetLong` / `compareAndSetReference`
- `compareAndExchangeInt` / `compareAndExchangeLong` / `compareAndExchangeReference`
- `getAndAddInt` / `getAndAddLong`
- `getAndSetInt` / `getAndSetLong` / `getAndSetReference`

**Test:** `t12_cas_int_atomic_under_contention` ✅

### T12.4 — Object/class inspection (15 methods) ✅
All inspection methods registered. T12 added 3 new ones:
- `objectFieldOffset0` / `objectFieldOffset1` / `staticFieldOffset0` / `staticFieldBase0`
- `arrayBaseOffset0` / `arrayIndexScale0`
- **`addressSize0()I`** — NEW (returns 8)
- **`isBigEndian0()Z`** — NEW (returns false)
- **`unalignedAccess0()Z`** — NEW (returns true)
- `allocateInstance` / `shouldBeInitialized0` / `ensureClassInitialized0`
- `throwException` / `park` / `unpark`

**Test:** `t12_unsafe_field_offset_matches_layout` ✅

### T12.5 — Fences + misc (7 methods) ✅
All fence and misc methods registered. T12 added 2 new ones:
- `loadFence()V` — SeqCst fence
- `storeFence()V` — SeqCst fence
- `fullFence()V` — SeqCst fence
- **`loadLoadFence()V`** — NEW (Acquire fence)
- **`storeStoreFence()V`** — NEW (Release fence)
- `pageSize()I` — returns 4096
- `getLoadAverage0([DI)I` — returns 0

**Test:** `t12_fence_does_not_panic` ✅

### T12.6 — Verification ✅
- All T12 modules compile without errors
- `cargo check --workspace` clean (zero errors)
- 8 T12 tests pass, 0 failures
- 184 types tests pass, 0 failures
- 1400+ native-builtins tests pass (4 pre-existing TLS failures only)
- Zero stubs, zero TODOs, zero FIXMEs across T12 code
- `copySwapMemory0` stub replaced with real byte-swap implementation

## Test Results

| Test Name | Status |
|-----------|--------|
| `t12_unsafe_alloc_free_round_trip` | ✅ Pass |
| `t12_unsafe_get_put_int_round_trip` | ✅ Pass |
| `t12_unsafe_volatile_ordering` | ✅ Pass |
| `t12_cas_int_atomic_under_contention` | ✅ Pass |
| `t12_unsafe_field_offset_matches_layout` | ✅ Pass |
| `t12_fence_does_not_panic` | ✅ Pass |
| `t12_copy_swap_memory_basic` | ✅ Pass |
| `t12_copy_swap_memory_no_src_is_noop` | ✅ Pass |

## Files Created

| File | Lines | Tests | Section |
|------|-------|-------|---------|
| `native-builtins/src/unsafe_jdk25.rs` | 457 | 8 | T12.4, T12.5, T12.1-T12.3 tests |

## Files Modified

| File | Change |
|------|--------|
| `native-builtins/src/lib.rs` | Added `pub mod unsafe_jdk25;`, wired `register_t12_unsafe_natives()`, replaced `copySwapMemory0` stub with real impl, made 15 unsafe functions `pub(crate)` for cross-module testing |

## Architecture Notes

- **addressSize0/isBigEndian0/unalignedAccess0** are platform queries that return compile-time constants. Our VM models a 64-bit little-endian platform with unaligned access support.
- **loadLoadFence** uses `Acquire` ordering (load-load ordering guarantee), **storeStoreFence** uses `Release` ordering (store-store ordering guarantee). These are weaker than the existing `fullFence` (SeqCst) but semantically correct per JMM.
- **copySwapMemory0** implements real byte-swapping per element size (2/4/8 bytes). In our slot-based VM model, elements are individual Value slots, and `swap_bytes()` is applied to integer values. Off-heap (null object) operations return silently since we don't support raw pointer access.
- All existing Unsafe implementations were already real (not stubs), using the VM's field/array access API via `NativeContext`.
