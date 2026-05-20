# NEW-18 — Panama `Linker` upcall completeness

## Goal

Replace CratonVM's hand-rolled FFI dispatch (max 8 args, integer-only
register classification, no variadic, no structs-by-value, broken
upcalls) with a `libffi`-backed implementation that is **ABI-correct
on every supported platform** for downcalls, upcalls, variadic calls,
and struct-by-value.

**Definition of done (per roadmap NEW-18):** a Java program that calls
real native libraries via Panama FFI and gets correct results — proven
by tests that exercise arbitrary-arity downcalls, mixed int/float args,
variadic `snprintf`, struct-by-value passing, and round-trip upcalls
where C code calls back into Java.

## Existing infrastructure

- `native-builtins/src/panama.rs` — Panama natives:
  - `pe_downcall_invoke` — current dispatcher, 0..8 args, all u64.
    **Wrong for floats** (`float`/`double` should land in `xmm`
    registers, not `rdi`/`rsi`).
  - `pe_upcall_handle` — registers a callback, allocates one of 64
    pre-generated trampolines from `_upcall_t00..63`. The trampolines
    currently return 0 unconditionally (`trampoline_dispatch`) — i.e.
    upcalls don't actually fire from C into Java.
  - `pe_struct_layout` / `pe_union_layout` / `pe_sequence_layout` —
    layout helpers (already correct).
- `NativeMemoryTable` (NEW-17) for arena allocations.
- LAYOUT_* constants in `native-api/src/ffi.rs` for layout kinds.
- `libffi = "3.2"` — newly added; `cargo check` confirms it builds on
  this host. Provides:
  - `libffi::middle::Cif` — CIF builder for arbitrary signatures.
  - `libffi::middle::Type` — primitive + struct types (recursive).
  - `Cif::call(CodePtr, &[Arg])` — make a downcall.
  - `Cif::call_variadic` for variadic.
  - `libffi::middle::Closure` — generates a real extern "C" function
    pointer that invokes a Rust closure when called from C — exactly
    what we need for upcalls.

## What is broken / missing

1. **Integer/float register confusion.** Every arg is currently passed
   as `u64`. A signature like `(int, double, int)` is dispatched
   through `extern "C" fn(u64, u64, u64) -> u64` — wrong on every ABI.
2. **8-arg ceiling.** Anything past 8 args returns
   `IllegalStateException`. Real C APIs (e.g. `XCreateWindow`,
   `gtk_init`-style helpers) take more.
3. **No variadic.** `printf`, `snprintf`, `execl`, `open` (3-arg form)
   cannot be called.
4. **No struct-by-value.** Passing a `struct timespec` or returning a
   `struct stat` is impossible.
5. **Broken upcalls.** `_upcall_tNN` trampolines do `return 0;` —
   the dispatch table is read but the Java target is never invoked
   from native code. The only working upcall path is when *Java itself*
   calls `UpcallStub.invoke([Object])`, which is not a real callback.

## Approach

Rebuild the downcall and upcall paths on top of `libffi::middle`:

- A pure-Rust helper module `panama_libffi` translates CratonVM's
  layout kinds + descriptor objects into `libffi::middle::Type`
  (recursively for structs/unions/sequences).
- `pe_downcall_invoke` builds a `Cif`, marshals each argument to a
  typed staging buffer, and calls `Cif::call`. Return value is read
  back from the typed result buffer.
- Variadic: `Cif::new_variadic(fixed, total, ret)` then `Cif::call`.
- Structs-by-value: `Type::structure([...])`. Argument staging
  copies the bytes from the MemorySegment into a per-call buffer.
- Upcalls: `libffi::middle::Closure::new(cif, callback, userdata)`
  produces a real extern "C" code pointer; the user data is the slot
  index, the callback fans out to the Java target via
  `pe_upcall_invoke`-style dispatch on a thread-local `NativeContext`.

Thread-local `NativeContext` is the only tricky bit for closures, but
CratonVM already exposes `NativeContext::invoke_virtual`. We can stash
a non-owning pointer to the active `NativeContext` in a thread-local
`Cell<*mut dyn NativeContext>` for the duration of any downcall, so
closures called *during a downcall* (the only legal path in Panama)
re-enter the live context.

## Checkpoints

### CP1 — `panama_libffi` helper module

**File:** `native-builtins/src/panama_libffi.rs` (new).

Public surface:

```rust
pub fn layout_to_type(ctx: &dyn NativeContext, layout: ObjectRef) -> Type;
pub fn read_layout_kind(ctx: &dyn NativeContext, layout: ObjectRef) -> i32;
pub fn descriptor_param_layouts(ctx: &dyn NativeContext, desc: ObjectRef) -> Vec<ObjectRef>;
pub fn descriptor_return_layout(ctx: &dyn NativeContext, desc: ObjectRef) -> Option<ObjectRef>;

/// Marshal a Java `Value` into a typed scratch slot for libffi.
/// Returns the byte size occupied; appends the storage to `scratch`.
pub fn marshal_arg(
    ctx: &mut dyn NativeContext,
    layout_kind: i32,
    arg: &Value,
    scratch: &mut Vec<u8>,
) -> Result<ArgStorage, MethodCallFailed>;

/// Read a libffi return slot back as a Java Value.
pub fn unmarshal_return(layout_kind: i32, slot: &[u8]) -> Value;
```

Internal: a `LayoutType` enum with `Primitive(i32)` / `Struct(Vec<LayoutType>, total_size, alignment, offsets)` / `Pointer`. Translation reads
the layout's field 0 (kind), and for STRUCT layouts walks field 2
(member layouts) recursively.

**Verify:** `cargo check -p cratonvm-native-builtins`.

### CP2 — Rewrite `pe_downcall_invoke` over libffi

**File:** `native-builtins/src/panama.rs::pe_downcall_invoke`.

Drop the 0..8 match cascade. New flow:

1. Read function pointer + descriptor (existing).
2. Build `param_types: Vec<Type>` from `param_layouts`.
3. Build `ret_type: Type` from `return_layout` (`Type::void()` if -1).
4. Allocate per-arg storage buffers (correct size + alignment).
5. Marshal each Java arg into its slot; build `Vec<Arg>`.
6. `Cif::new(param_types, ret_type)` and `.call(CodePtr::from_ptr(fn_addr as *const _), args)`.
7. Read the return slot back through `unmarshal_return`.

Return-value handling:
- void → `None` (current Ok(Some(Value::Object(None))))
- byte/bool/short/char → widened to `Value::Int`
- int → `Value::Int`
- long/address → `Value::Long`
- float → `Value::Float`
- double → `Value::Double`
- struct → MemorySegment wrapping a fresh Arena allocation
  containing the copied result.

Error handling: any layout we can't translate → `IllegalStateException`
with the layout kind in the message. Null function pointer continues
to use `validated_fn_ptr` (kept as a cheap pre-check; libffi's
`CodePtr` doesn't validate alignment).

**Verify:** `cargo check`. Existing libffi-related tests + the new
NEW-18 tests must compile.

### CP3 — Variadic downcalls

**Files:** `panama.rs` — accept a sentinel descriptor flag for variadic.

The Java-side `Linker.Option.firstVariadicArg(int n)` records the
fixed-arg count. CratonVM's `FunctionDescriptor` synthetic doesn't
carry this option today. We add a 3rd field to FunctionDescriptor:

- field 2 : `Int` — index of the first variadic arg, or -1 (default).

`Linker.downcallHandle(MemorySegment, FunctionDescriptor, Linker.Option...)`
overload sets it via the option array. We add a registration for
`Linker.Option.firstVariadicArg(I)Ljava/lang/foreign/Linker$Option;`
that allocates a 1-field synthetic carrying the int, and the
`downcallHandle(...)` overload reads it back.

`pe_downcall_invoke`: when the descriptor has a non-negative variadic
index `k`, call `Cif::new_variadic(param_types, ret_type, k as usize)`
instead of `Cif::new(...)`.

### CP4 — Real upcalls via libffi closures

**Files:** `panama.rs::pe_upcall_handle`, drop the static trampoline pool.

New plan:
- `pe_upcall_handle(target, descriptor, arena)` builds a `Cif` from the
  descriptor, then `libffi::middle::Closure::new(cif, callback, userdata)`.
  The closure's `code_ptr()` is the real extern "C" address we hand
  back as a MemorySegment.
- `userdata` is a heap-allocated `UpcallContext { slot_id, param_kinds,
  return_kind }`. It lives until the arena that owns the segment is
  closed — we register a NEW-17 cleaner on the segment that drops the
  Closure and the userdata.
- The libffi callback runs on whatever thread C code chose. It
  consults a thread-local `Cell<Option<*mut dyn NativeContext>>` set
  by the surrounding downcall (the *only* legal way for an upcall to
  fire is from within a downcall on a Panama-registered callback);
  when set, it dispatches `target.invoke([...])` through
  `NativeContext::invoke_virtual`. When unset (callback fired
  outside of any downcall, e.g. signal handler), it logs and returns
  the zero/null of the return type.

The static `_upcall_t00..63` trampoline pool, the `UPCALL_TRAMPOLINE_FNS`
array, and `MAX_UPCALL_TRAMPOLINES` are deleted — libffi closures replace
all of them with real, ABI-correct trampolines.

### CP5 — Tests

Three real-FFI integration tests in `native-builtins/tests/new18_panama.rs`
(new file) using the C library functions guaranteed by libc:

1. **`new18_downcall_strlen`** — `strlen("hello world")` returns 11.
   Exercises 1-arg pointer downcall + long return.
2. **`new18_downcall_snprintf_variadic`** — `snprintf(buf, len, "%d %s",
   42, "abc")` and verify the buffer contents. Exercises variadic.
3. **`new18_downcall_mixed_int_float`** — call a C helper that returns
   `int + (int)double` to prove that floats land in `xmm` and ints in
   integer registers. Use `pow(2.0, 3.0) = 8.0` from `libm`.
4. **`new18_upcall_qsort_callback`** — call `qsort` on a 5-int array
   with a comparator implemented as a Java MethodHandle target. After
   the call, assert the array is sorted ascending. Exercises the full
   round-trip upcall path: C code invokes a Java callback inside a
   downcall.

These run on Linux/macOS/Windows since libc, libm, and `qsort` are
universal. On Windows we use `ucrtbase`/`msvcrt` symbol resolution
through the existing `find_native_symbol` path.

### CP6 — Roadmap update

Mark NEW-18 ✅ DELIVERED with a summary of:
- libffi-backed downcall/upcall pipeline.
- Real ABI correctness across SysV + Win64.
- Variadic + struct-by-value support.
- Test names and what each proves.

## Files touched

| File | Change |
|------|--------|
| `native-builtins/Cargo.toml` | + `libffi = "3.2"` (already added) |
| `native-builtins/src/panama_libffi.rs` | NEW — layout↔Type bridge + arg marshaling |
| `native-builtins/src/panama.rs` | Rewrite `pe_downcall_invoke` + `pe_upcall_handle`; delete pool |
| `native-builtins/src/lib.rs` | `mod panama_libffi;` |
| `native-builtins/tests/new18_panama.rs` | NEW — integration tests |
| `docs/roadmap.md` | NEW-18 ✅ DELIVERED |

## Invariants preserved

- All `unsafe` calls remain isolated behind `validated_fn_ptr` /
  libffi's own validation. No raw transmute of Java values to function
  pointers.
- `NativeMemoryTable` continues to own all arena-allocated memory; the
  cleaner integration from NEW-17 ensures upcall closures + userdata
  are released when their owning arena closes.
- Variadic flag is opt-in via a new descriptor field that defaults to
  `-1`, so existing code paths that don't construct variadic
  descriptors are byte-identical.
