# Interpreter Intrinsic Table — Implementation Contract

This is the SHARED CONTRACT for the parallel implementation of
`docs/feature_roadmap_interpreter_intrinsic_table.md`. Every agent MUST follow
the names, signatures, file paths, and table entries here EXACTLY so the
independently-written pieces integrate without conflict. Read the roadmap too.

Hard rule (project memory `feedback_no_synthetic_stubs`): NO synthetic stubs,
NO fake behavior. Every handler must be byte-for-byte behavior-identical to the
normal native/registry dispatch path. If you cannot implement something
correctly, delegate to `ctx.invoke(class, name, desc, args)` as a correctness
floor and leave a `// TODO(intrinsic): not yet a true fast path` comment.

## Crate dependency facts
- `InterpIntrinsic` lives in crate `cratonvm-native-api` (so `classloading`,
  `native-builtins`, and `vm` can all see it).
- `native-builtins` depends on `native-api` and `classloading`. Its native impl
  functions (`crate::lang_string::native_string_length`, etc.) are reachable
  crate-wide via `crate::<module>::<fn>` even when `pub(crate)`.
- `MethodCallResult` = `cratonvm_types::error::MethodCallResult`.
  `MethodCallResult = Result<Option<Value>, MethodCallFailed>`.
- `Value` = `cratonvm_types::Value`.
- `NativeContext` / `NativeCallback` = in `cratonvm_native_api`.
  `NativeCallback = fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult`.

## Files (owner agent in brackets)
- `native-api/src/intrinsic.rs` NEW — `InterpIntrinsic` enum.            [FOUNDATION]
- `native-api/src/lib.rs` — add `pub mod intrinsic; pub use intrinsic::InterpIntrinsic;` [FOUNDATION]
- `classloading/src/resolution.rs` — `CachedInvokeTarget::Intrinsic` variant + Debug/is_stale arms. [FOUNDATION]
- `native-builtins/src/intrinsics/mod.rs` NEW — `lookup()`, `dispatch()`, submodule decls. [FOUNDATION]
- `native-builtins/src/lib.rs` — add `pub mod intrinsics;`               [FOUNDATION]
- `vm/src/runtime/env_cache.rs` — add `intrinsics_disabled()` flag.        [FOUNDATION]
- `vm/src/runtime/interpreter.rs` — IC populate + dispatch integration + hit counter. [FOUNDATION]
- `native-builtins/src/intrinsics/object.rs` NEW                          [OBJECT]
- `native-builtins/src/intrinsics/string.rs` NEW                          [STRING]
- `native-builtins/src/intrinsics/system.rs` NEW                          [SYSTEM]
- `native-builtins/src/intrinsics/stringbuilder.rs` NEW                   [STRINGBUILDER]
- `native-builtins/src/intrinsics/integer.rs` NEW                         [INTEGER]
- `native-builtins/src/intrinsics/long.rs` NEW                            [LONG]
- `native-builtins/src/intrinsics/math.rs` NEW                            [MATH]
- tests — `vm/tests/intrinsic_diff.rs` + Java sources under `vm/tests/`.   [TESTS]

## InterpIntrinsic enum (native-api/src/intrinsic.rs)
```rust
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum InterpIntrinsic {
    ObjectGetClass, ObjectHashCode,
    StringLength, StringCharAt, StringIsEmpty,
    SystemArraycopy,
    StringBuilderAppendString, StringBuilderAppendInt, StringBuilderAppendChar,
    StringBuilderAppendLong, StringBuilderAppendBool, StringBuilderAppendObject,
    StringBuilderToString, StringBuilderLength,
    IntegerValueOf, IntegerIntValue, IntegerParseInt,
    LongValueOf, LongLongValue, LongParseLong,
    MathAbsInt, MathAbsLong, MathAbsDouble,
    MathMinInt, MathMaxInt, MathMinLong, MathMaxLong, MathSqrt,
}
```

## Handler signature (every handler fn, in every group file)
```rust
pub fn intrinsic_xxx(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[cratonvm_types::Value],
) -> cratonvm_types::error::MethodCallResult
```
`args` for an INSTANCE method is `[receiver, param0, ...]`; for a STATIC method
`[param0, ...]` — exactly what the native registry path passes. Handlers must
delegate to the existing crate-local `native_*` function for that method when
one exists (look it up: grep `registry.register(... "<name>" ... )` in
native-builtins). For pure arithmetic (Math) implement inline by reading the
args. Add `#[cfg(test)]` unit tests for each handler in the same file.

## Intrinsic table — (class, name, descriptor) -> InterpIntrinsic -> handler fn

Static (S) calls have no receiver guard; Virtual (V) calls need the receiver
class-id guard (see roadmap 3.4).

OBJECT (object.rs) — both V:
- java/lang/Object  getClass  ()Ljava/lang/Class;  -> ObjectGetClass  -> intrinsic_object_get_class
- java/lang/Object  hashCode  ()I                  -> ObjectHashCode  -> intrinsic_object_hash_code

STRING (string.rs) — all V (java/lang/String is final):
- java/lang/String  length  ()I   -> StringLength  -> intrinsic_string_length
- java/lang/String  charAt  (I)C  -> StringCharAt  -> intrinsic_string_char_at
- java/lang/String  isEmpty ()Z   -> StringIsEmpty -> intrinsic_string_is_empty

SYSTEM (system.rs) — S:
- java/lang/System  arraycopy  (Ljava/lang/Object;ILjava/lang/Object;II)V -> SystemArraycopy -> intrinsic_system_arraycopy

STRINGBUILDER (stringbuilder.rs) — class java/lang/StringBuilder, all V:
- append  (Ljava/lang/String;)Ljava/lang/StringBuilder;  -> StringBuilderAppendString -> intrinsic_sb_append_string
- append  (I)Ljava/lang/StringBuilder;                   -> StringBuilderAppendInt    -> intrinsic_sb_append_int
- append  (C)Ljava/lang/StringBuilder;                   -> StringBuilderAppendChar   -> intrinsic_sb_append_char
- append  (J)Ljava/lang/StringBuilder;                   -> StringBuilderAppendLong   -> intrinsic_sb_append_long
- append  (Z)Ljava/lang/StringBuilder;                   -> StringBuilderAppendBool   -> intrinsic_sb_append_bool
- append  (Ljava/lang/Object;)Ljava/lang/StringBuilder;  -> StringBuilderAppendObject -> intrinsic_sb_append_object
- toString ()Ljava/lang/String;                          -> StringBuilderToString     -> intrinsic_sb_to_string
- length  ()I                                            -> StringBuilderLength       -> intrinsic_sb_length

INTEGER (integer.rs) — class java/lang/Integer:
- valueOf  (I)Ljava/lang/Integer;        S -> IntegerValueOf   -> intrinsic_integer_value_of
- intValue ()I                           V -> IntegerIntValue  -> intrinsic_integer_int_value
- parseInt (Ljava/lang/String;)I         S -> IntegerParseInt  -> intrinsic_integer_parse_int

LONG (long.rs) — class java/lang/Long:
- valueOf   (J)Ljava/lang/Long;          S -> LongValueOf    -> intrinsic_long_value_of
- longValue ()J                          V -> LongLongValue  -> intrinsic_long_long_value
- parseLong (Ljava/lang/String;)J        S -> LongParseLong  -> intrinsic_long_parse_long

MATH (math.rs) — class java/lang/Math, all S, all pure arithmetic (inline):
- abs (I)I  -> MathAbsInt    -> intrinsic_math_abs_int
- abs (J)J  -> MathAbsLong   -> intrinsic_math_abs_long
- abs (D)D  -> MathAbsDouble -> intrinsic_math_abs_double
- min (II)I -> MathMinInt    -> intrinsic_math_min_int
- max (II)I -> MathMaxInt    -> intrinsic_math_max_int
- min (JJ)J -> MathMinLong   -> intrinsic_math_min_long
- max (JJ)J -> MathMaxLong   -> intrinsic_math_max_long
- sqrt (D)D -> MathSqrt      -> intrinsic_math_sqrt

## native-builtins/src/intrinsics/mod.rs (FOUNDATION writes)
```rust
pub mod object; pub mod string; pub mod system; pub mod stringbuilder;
pub mod integer; pub mod long; pub mod math;

use cratonvm_native_api::{InterpIntrinsic, NativeContext};
use cratonvm_types::{Value, error::MethodCallResult};

/// One-time resolution: static (class,name,desc) -> intrinsic kind.
pub fn lookup(class: &str, name: &str, desc: &str) -> Option<InterpIntrinsic> {
    use InterpIntrinsic::*;
    Some(match (class, name, desc) {
        ("java/lang/Object", "getClass", "()Ljava/lang/Class;") => ObjectGetClass,
        // ... all entries ...
        _ => return None,
    })
}

/// Steady-state dispatch — no lock, no hashmap, no descriptor parse.
pub fn dispatch(kind: InterpIntrinsic, ctx: &mut dyn NativeContext, args: &[Value])
    -> MethodCallResult
{
    use InterpIntrinsic::*;
    match kind {
        ObjectGetClass => object::intrinsic_object_get_class(ctx, args),
        // ... all 28 arms ...
    }
}
```
NOTE: the roadmap says `phf::Map`. `phf` is NOT a workspace dependency — do NOT
add it. A plain `match` in `lookup()` is faster (direct branches, no hashing)
and dependency-free. Resolution happens once per call site, so this is fine.

## CachedInvokeTarget::Intrinsic (classloading/src/resolution.rs)
Add a variant. Suggested shape (FOUNDATION decides final form):
```rust
Intrinsic {
    kind: cratonvm_native_api::InterpIntrinsic,
    /// the intrinsic handler, callable exactly like a NativeCallback
    callback: NativeCallback,
    num_params: u16,
    /// Some(class) => virtual call, guard receiver class-id; None => static.
    receiver_class_id: Option<ClassId>,
    gate: RedefineGate,
},
```
Wire it into `is_stale()` and the `Debug` impl.

## Interpreter integration (FOUNDATION) — vm/src/runtime/interpreter.rs
- env flag: `vm/src/runtime/env_cache.rs` add `intrinsics_disabled()` reading
  `CRATONVM_DISABLE_INTRINSICS` (follow the `disable_jit()` OnceLock pattern).
- `populate_invoke_cache` (~line 10947, invokestatic/special): before building
  `CachedInvokeTarget::Native`, if `!intrinsics_disabled()` probe
  `cratonvm_native_builtins::intrinsics::lookup(&class_name,&method_name,&descriptor)`;
  on hit build `CachedInvokeTarget::Intrinsic { receiver_class_id: None, .. }`
  (callback = the native callback you already resolved, OR a thin dispatch
  shim — keep both `kind` and a directly-callable `NativeCallback`).
- `populate_virtual_invoke_cache` (~line 13893, invokevirtual/interface): probe
  the table keyed on the RESOLVED declaring class (the class that actually
  provides the body for this receiver) — NOT the static cp class. This makes an
  overriding subclass (e.g. a class overriding `hashCode`) correctly miss the
  intrinsic. On hit build `Intrinsic { receiver_class_id: Some(actual), .. }`.
- `execute_invokestatic_cached` (~11098) and `execute_invokevirtual_cached`
  (~13574): add a `CachedInvokeTarget::Intrinsic` match arm. For virtual,
  guard `actual_class_id == receiver_class_id` then fall to CacheMiss on
  mismatch (model on the `VirtualNative` arm ~13744). Pop args exactly as the
  `Native`/`VirtualNative` arms do and call `intrinsics::dispatch(kind, ctx,
  &args)` (or the stored callback). Increment a `static AtomicU64`
  `INTRINSIC_HITS` counter and expose a `pub fn intrinsic_hit_count() -> u64`.
- Investigate `execute_invokevirtual_vtable_fast` (~line 4413 caller) — make
  sure it does not shadow the intrinsic for String/StringBuilder/Object virtual
  calls. If it does, add the intrinsic check there or ensure the cached path
  still runs first for intrinsic-eligible sites.
- When `intrinsics_disabled()` is true, NEVER populate an `Intrinsic` entry —
  this is the differential-test off-switch.

## Testing (TESTS) — vm/tests/intrinsic_diff.rs
- A Java program (or several) exercising every intrinsic above with a
  randomized/edge-case input matrix, that self-checks expected values and
  prints a deterministic result line.
- A Rust integration test that runs that program TWICE — once normally, once
  with `CRATONVM_DISABLE_INTRINSICS=1` — and asserts identical output/exit.
- A virtual-dispatch guard test: a subclass overriding `hashCode` must NOT hit
  the intrinsic (assert via `intrinsic_hit_count()` if reachable, else via
  behavior — overridden hashCode returns its own value).
- A megamorphic test: a call site seeing many receiver classes must not
  livelock and must produce correct results.
- `arraycopy` exception parity: NPE / ArrayStoreException /
  ArrayIndexOutOfBoundsException thrown identically with intrinsics on vs off.
- Follow existing patterns under `vm/tests/` for compiling+running Java.
