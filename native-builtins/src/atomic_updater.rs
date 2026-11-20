// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! T19.H5 — `AtomicReferenceFieldUpdater` / `AtomicIntegerFieldUpdater` /
//! `AtomicLongFieldUpdater` natives.
//!
//! Quarkus / JBoss code (notably `org.jboss.logmanager.ExtHandler`) calls
//! `AtomicReferenceFieldUpdater.newUpdater(ExtHandler.class, Handler.class,
//! "next")` from a `<clinit>`. The real JDK implementation reaches into
//! `java.lang.reflect.Field` via `Reflection.getCallerClass()` and performs
//! a strict access + type-equality check that uses cached
//! `Class<?>` references on the synthetic Field. CratonVM's synthetic
//! Field mirror does not preserve the exact JDK private layout, so the
//! reflective cast trips a `ClassCastException` in `<clinit>`, which is
//! silently swallowed but leaves the static `next` slot pointing at a
//! stub that NPEs on first publish.
//!
//! Rather than try to align our synthetic Field with the JDK private
//! layout (which would force every reflective callsite to track), we
//! register native overrides for the three `*FieldUpdater.newUpdater`
//! factories. The native:
//!
//! 1. Validates the `Class<U>`/`Class<V>`/field-name arguments
//!    (rejecting nulls, control bytes, path-traversal patterns, names
//!    that are too long, etc.).
//! 2. Looks up the field's `FieldMetadata` on the target class via
//!    `ctx.declared_fields`.
//! 3. Verifies the field exists, is non-static, and (for the reference
//!    variant) the declared field type matches the value class.
//! 4. Allocates a synthetic `*FieldUpdaterImpl` whose only job is to
//!    remember (a) the absolute heap slot, (b) the access-flag bits,
//!    and (c) the descriptor type.
//!
//! The accessor methods (`get`, `set`, `compareAndSet`, `getAndSet`,
//! `lazySet`, `getAndAdd`, etc.) are then registered against the
//! synthetic impl class and dispatch through volatile / CAS field
//! primitives on the receiver — the exact same primitives that
//! `AtomicReference` / `AtomicInteger` / `AtomicLong` already use.
//!
//! ## Synthetic instance layout
//!
//! All three `*FieldUpdaterImpl` classes share a 4-slot layout so the
//! same accessor implementation can read it uniformly:
//!
//! | Slot | Name        | Type    | Purpose                                              |
//! |------|-------------|---------|------------------------------------------------------|
//! | 0    | tclassId    | Int     | ClassId of the target class (the `U` of `<U,V>`).    |
//! | 1    | slotIndex   | Int     | Absolute heap slot in `U` instances.                 |
//! | 2    | descTag     | Int     | 1=int, 2=long, 3=reference (compile-time choice).    |
//! | 3    | vclassId    | Int     | ClassId of the value class (or 0 for int/long).      |
//!
//! `descTag` lets a single accessor handle every variant without
//! re-parsing the descriptor each call.
//!
//! ## Security posture
//!
//! * Field-name input is strictly validated: empty, longer than 256
//!   chars, containing path separators (`/`, `\`, `:`), `..` traversal
//!   sequences, or any control byte (< 0x20 or 0x7F) is rejected with
//!   `IllegalArgumentException` before any heap activity -- only under
//!   `CRATONVM_FIELD_UPDATER_JDK_RULES=0`; by default (JDK 25) such a name
//!   is a missing field, `RuntimeException(NoSuchFieldException)`.
//! * Static fields are rejected — `*FieldUpdater` is documented to
//!   target instance fields only, and a stale static-field reference
//!   could otherwise be coerced into a cross-instance memory probe.
//! * Final fields are rejected (matches JDK behaviour).
//! * Type-mismatch in the reference variant is reported as
//!   `ClassCastException` so callers see the same behaviour as on real
//!   HotSpot.
//! * No part of the impl object's heap slots is exposed to Java code via
//!   reflection — the synthetic class is internal-use-only.
//!
//! ## `--real-jdk`: this whole module is NOT registered (2026-08-12)
//!
//! Everything above describes **Compatible** mode and is unchanged there.
//! Under [`CompatibilityMode::JdkOnly`] `register_atomic_updater_natives`
//! registers nothing at all — see the guard at the top of it.
//!
//! The reason is measured, not argued.
//! `docs/feature-designs/native-dispatch-architecture.md` §3.3 records that
//! `java.sql.SQLException` holds
//! `private static final AtomicReferenceFieldUpdater<SQLException,SQLException> nextUpdater`,
//! so its `<clinit>` runs `newUpdater`, so `alloc_impl` asks for
//! `…FieldUpdater$RustJvmImpl` — a class **no JDK image declares** — and strict
//! mode correctly refuses to fabricate it. The refusal surfaces at the
//! application as `NoClassDefFoundError` and takes the **entire `java.sql`
//! package** with it: all JDBC, every ORM, every pool, plus H2's MVStore
//! (`AtomicIntegerFieldUpdater`, reached with no JDBC at all) and — because
//! `org.h2.message.DbException extends SQLException` — H2's *exception
//! identity*, which is how `TestStringUtils` reports "expected `DbException`,
//! got `NoClassDefFoundError`".
//!
//! **The refusal is correct; the survival of its caller is the defect.** The
//! three `…$RustJvmImpl` accessor blocks were already dropped in strict mode by
//! [`cratonvm_native_api::no_image_receiver::NO_IMAGE_JDK_RECEIVERS`], which
//! lists all three impl names and re-tags a `Bridge` on them to
//! `SyntheticStub`. That central rule cannot reach the `newUpdater` factories
//! or the base-class accessors, because those are registered on
//! `java/util/concurrent/atomic/Atomic*FieldUpdater` — names every image DOES
//! declare. So strict mode kept exactly the half that mints the class and
//! dropped exactly the half that would have used it.
//!
//! Dropping the base-class accessors matters independently of `newUpdater`.
//! `getAndIncrement` / `getAndDecrement` / `addAndGet` / `incrementAndGet` /
//! `decrementAndGet` / `getAndAdd` are **concrete on the abstract base** in the
//! JDK and are *not* overridden by `Atomic*FieldUpdaterImpl`. CratonVM's
//! virtual dispatch walks the superclass chain and lets a native on a parent
//! shadow inherited bytecode when the receiver declares no override of its own
//! (`vm/src/runtime/interpreter/dispatch_virtual.rs`, the `receiver_has_own_bytecode`
//! guard). A real JDK `AtomicIntegerFieldUpdaterImpl` receiver reaching
//! `native_aifu_get_and_increment` would have its `offset`/`cclass` fields read
//! as `FU_SLOT_FIELD_INDEX`, `impl_slot` would answer `None`, and
//! `.unwrap_or(0)` would fetch-add slot **0** of the target. That is a silent
//! wrong-slot write, not an error — so leaving the base-class rows registered
//! while removing `newUpdater` would have been strictly worse than either.
//!
//! Why *not* keep the native and return a real object (the other candidate
//! shape §6 of the readiness record names): building a real
//! `Atomic*FieldUpdaterImpl` means calling the JDK's own private constructor
//! with the offset the JDK's own `Unsafe.objectFieldOffset` computes — i.e.
//! re-implementing the bytecode strict mode exists to run, with a second copy
//! to keep in step. And the reason this native exists in the first place does
//! not apply in strict mode; see below.
//!
//! ### Why the native exists, and why removing it is safe *here* and only here
//!
//! `git log --follow` puts the file's whole history in the tree's first commit
//! (`a6dc911ed`, "Open-source initial commit"), so the only statement of the
//! original motivation is the module header above, and it names the workaround
//! precisely: the JDK's reflective `newUpdater` "reaches into
//! `java.lang.reflect.Field` … CratonVM's **synthetic** Field mirror does not
//! preserve the exact JDK private layout, so the reflective cast trips a
//! `ClassCastException` in `<clinit>`". Every later commit on this file
//! (`fe52db3a5`, `686183560`, `636b58278`, `b291ffc3d`, `dc55e8057`) extends or
//! repairs the *accessors*; none revisits that premise.
//!
//! The premise is about a **synthetic** `Field`. Under `--real-jdk` there is no
//! synthetic `Field`: `java.lang.reflect.Field` is loaded from the image and
//! `declared_fields` reports its real classfile `access_flags`. So the
//! `ClassCastException` this native was written to dodge cannot be resurrected
//! by a change that only stops registering under `JdkOnly` — Compatible mode,
//! where the premise may still hold, is byte-for-byte untouched. That is why
//! this is a **mode-conditional registration and not a deletion**.
//!
//! What is NOT established here, and is the thing to measure: whether the JDK's
//! own `Atomic*FieldUpdaterImpl` bytecode then works on this VM end to end. It
//! needs `Reflection.getCallerClass()` (registered on the real
//! `jdk/internal/reflect/Reflection`, a genuine `ACC_NATIVE` bridge that
//! survives strict mode) and `Unsafe.objectFieldOffset(Field)` +
//! `compareAndSetReference`. The last of those has a documented failure mode:
//! when `objectFieldOffset1` cannot resolve a (class, field) pair it **mints a
//! synthetic offset** and routes loads/stores through a side table
//! (`SYNTHETIC_OFFSET_BASE`, `lib.rs`), which is invisible to a plain
//! `getfield` of the same field. `regression-suite/src/RJdkSqlPackage.java`
//! exists to catch exactly that: it asserts that the updater and a direct field
//! read/write address **one** storage location, in both directions.

#![allow(clippy::needless_pass_by_value)]

use crate::lang_class::ClassTextName;
use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::{ClassId, ObjectRef, Value};

use crate::lang_class::mirror_class_id;

// ---------------------------------------------------------------------------
// Class names (impl shells we own)
// ---------------------------------------------------------------------------

/// Public-facing factory classes (real JDK names).  Their `newUpdater`
/// statics are overridden; nothing else on these classes is touched.
const CLS_REF_FIELD_UPDATER: &str = "java/util/concurrent/atomic/AtomicReferenceFieldUpdater";
const CLS_INT_FIELD_UPDATER: &str = "java/util/concurrent/atomic/AtomicIntegerFieldUpdater";
const CLS_LONG_FIELD_UPDATER: &str = "java/util/concurrent/atomic/AtomicLongFieldUpdater";

/// Synthetic concrete classes returned by `newUpdater`.  Method natives
/// dispatch on these names; the JDK has its own private impl classes
/// with similar names, but we avoid colliding with the real ones by
/// using the cratonvm-internal `$RustJvmImpl` suffix.
pub const CLS_REF_FIELD_UPDATER_IMPL: &str =
    "java/util/concurrent/atomic/AtomicReferenceFieldUpdater$RustJvmImpl";
pub const CLS_INT_FIELD_UPDATER_IMPL: &str =
    "java/util/concurrent/atomic/AtomicIntegerFieldUpdater$RustJvmImpl";
pub const CLS_LONG_FIELD_UPDATER_IMPL: &str =
    "java/util/concurrent/atomic/AtomicLongFieldUpdater$RustJvmImpl";

// ---------------------------------------------------------------------------
// Synthetic-impl slot layout
// ---------------------------------------------------------------------------

pub const FU_SLOT_TCLASS_ID: usize = 0;
pub const FU_SLOT_FIELD_INDEX: usize = 1;
pub const FU_SLOT_DESC_TAG: usize = 2;
pub const FU_SLOT_VCLASS_ID: usize = 3;
/// JIT round 21 wave 1, lane updaters: the JDK's `cclass` -- the caller,
/// when the field is `protected` and the caller a subclass of `tclass` in
/// another package; else `tclass`. Always written by `build_updater` (`tclass`
/// unless the protected rule, under [`field_updater_jdk_rules`], picks the
/// caller).
pub const FU_SLOT_CCLASS_ID: usize = 4;
pub const FU_NUM_SLOTS: usize = 5;

const DESC_TAG_INT: i32 = 1;
const DESC_TAG_LONG: i32 = 2;
const DESC_TAG_REF: i32 = 3;

// ---------------------------------------------------------------------------
// Field-name validation
// ---------------------------------------------------------------------------

/// Reject obviously dangerous field names. The JVMS allows almost any
/// UTF-8 sequence in a field name, but a *FieldUpdater's `String fieldName`
/// is application-supplied data that flows directly into a reflective
/// lookup; we want defense-in-depth against:
///
/// * Embedded NUL or other control bytes that would confuse downstream
///   logging / monitoring.
/// * `..` segments that could trick a stack-walk-based access check
///   into looking like a different package than intended.
/// * Path separators (`/`, `\`, `:`) — a real Java field name never
///   contains these, and rejecting them shrinks the attack surface.
/// HotSpot's helpful NPE for `newUpdater(null, ...)`, transcribed. Kept as a
/// constant so the three call sites cannot drift apart.
const NULL_TCLASS_NPE: &str = "java.lang.NullPointerException: Cannot invoke \"java.lang.Class.getDeclaredField(String)\" because \"tclass\" is null";

fn validate_field_name(name: &str) -> Result<(), RuntimeError> {
    if name.is_empty() {
        return Err(RuntimeError::IllegalArgumentException {
            message: "field name must not be empty".to_string(),
        });
    }
    if name.len() > 256 {
        return Err(RuntimeError::IllegalArgumentException {
            message: "field name too long".to_string(),
        });
    }
    if name.contains("..") {
        return Err(RuntimeError::IllegalArgumentException {
            message: "field name contains traversal sequence".to_string(),
        });
    }
    for b in name.bytes() {
        if b < 0x20 || b == 0x7F {
            return Err(RuntimeError::IllegalArgumentException {
                message: "field name contains control byte".to_string(),
            });
        }
        if b == b'/' || b == b'\\' || b == b':' {
            return Err(RuntimeError::IllegalArgumentException {
                message: "field name contains path separator".to_string(),
            });
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Argument helpers
// ---------------------------------------------------------------------------

fn arg_obj_or_npe(args: &[Value], idx: usize, what: &str) -> Result<ObjectRef, MethodCallFailed> {
    match args.get(idx) {
        Some(Value::Object(Some(o))) => Ok(*o),
        _ => Err(RuntimeError::NullPointerException {
            message: Some(format!("{what} is null")),
        }
        .into()),
    }
}

fn arg_obj(args: &[Value], idx: usize) -> Option<ObjectRef> {
    match args.get(idx) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    }
}

fn arg_value(args: &[Value], idx: usize) -> Value {
    args.get(idx).copied().unwrap_or(Value::Object(None))
}

// ---------------------------------------------------------------------------
// Descriptor → tag mapping
// ---------------------------------------------------------------------------

/// Determine which tag (int / long / reference) the field uses,
/// returning `None` if the descriptor is unsupported by the variant
/// (e.g. a `boolean` for an int updater is allowed, but `double` is not).
fn descriptor_tag_for_int_variant(desc: &str) -> Option<i32> {
    // AtomicIntegerFieldUpdater accepts only int-shaped fields:
    // boolean, byte, short, char, int.
    // (The JDK rejects long/double/object/float.)
    match desc {
        "Z" | "B" | "S" | "C" | "I" => Some(DESC_TAG_INT),
        _ => None,
    }
}

fn descriptor_tag_for_long_variant(desc: &str) -> Option<i32> {
    match desc {
        "J" => Some(DESC_TAG_LONG),
        _ => None,
    }
}

fn descriptor_is_reference(desc: &str) -> bool {
    desc.starts_with('L') && desc.ends_with(';') || desc.starts_with('[')
}

/// For reference variant, extract the internal class name from a
/// descriptor like `Ljava/lang/Object;` → `java/lang/Object`.  Returns
/// `None` for array descriptors (those don't have a single internal
/// class name; we keep them as raw and skip type-equality).
fn ref_descriptor_to_internal_name(desc: &str) -> Option<String> {
    if desc.starts_with('L') && desc.ends_with(';') {
        Some(desc[1..desc.len() - 1].to_string())
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Synthetic-impl allocation
// ---------------------------------------------------------------------------

fn alloc_impl(
    ctx: &mut dyn NativeContext,
    impl_class: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    match ctx.ensure_class_initialized(impl_class) {
        Ok(cid) => {
            let real = ctx.class_num_total_fields(cid);
            let n = FU_NUM_SLOTS.max(real);
            Ok(ctx.alloc_object(cid, n))
        }
        // Fall back to a synthetic class declaring `FU_NUM_SLOTS` fields
        // rather than `ClassId::new(0)` (`java/lang/Object`, zero declared
        // fields): an object with Object's id but a non-zero slot count is
        // an undersized layout the GC's `get_field` bounds guard rejects.
        //
        // Fallible since 2026-08-10 (Real-JDK wave 2, step 3): the
        // `…FieldUpdater$RustJvmImpl` classes this mints are declared by no
        // JDK image — the dead sweep names them as VM-minted classes wearing a
        // JDK name — so a strict run that gets one is running a synthetic field
        // updater in place of `java.util.concurrent.atomic`'s own bytecode. It
        // now refuses, naming the class.
        Err(_) => {
            let cid = crate::util_concurrent_ext::refused_class(ctx, impl_class, FU_NUM_SLOTS)?;
            Ok(ctx.alloc_object(cid, FU_NUM_SLOTS))
        }
    }
}

// ---------------------------------------------------------------------------
// Common newUpdater body
// ---------------------------------------------------------------------------

/// `tclass_mirror` — the `Class<U>` argument.
/// `field_name`     — the third argument (or second for int/long).
/// `expected`       — closure that returns Some(tag) if the field's
///                    descriptor is acceptable for this updater variant.
/// `impl_class`     — synthetic concrete impl name to allocate.
/// `vclass_mirror`  — Some(mirror) for reference variant only.
fn build_updater(
    ctx: &mut dyn NativeContext,
    tclass_mirror: ObjectRef,
    field_name: &str,
    expected: impl Fn(&str) -> Option<i32>,
    impl_class: &str,
    vclass_mirror: Option<ObjectRef>,
    // The reference variant was handed a NULL `vclass` (only under
    // `CRATONVM_FIELD_UPDATER_JDK_RULES`; `vclass_mirror` is then `None`).
    // JDK 25 runs the whole `try` (`getDeclaredField`, `ensureMemberAccess`,
    // `getType`) first and then fails `vclass != fieldClass` with the
    // message-less `ClassCastException`.
    vclass_null: bool,
    // HotSpot's wording for THIS variant's type refusal, transcribed:
    // "Must be integer type" / "Must be long type". The reference variant
    // never reaches it -- it leaves by `ClassCastException` -- and passes "".
    type_message: &str,
) -> MethodCallResult {
    // r21 wave 9 review (rev-w68-native, fixed in place): under
    // `CRATONVM_FIELD_UPDATER_JDK_RULES` no name is refused up front. JDK 25's
    // `tclass.getDeclaredField("")` (or `"a/b"`, `".."`, a 300-char name) just
    // finds no field: `RuntimeException(NoSuchFieldException(name))`, the
    // miss below. The name only ever meets a by-value comparison here. `=0`
    // keeps the old `IllegalArgumentException` screen.
    if !field_updater_jdk_rules() {
        validate_field_name(field_name).map_err(MethodCallFailed::from)?;
    }

    let tclass_id = mirror_class_id(ctx, tclass_mirror).ok_or_else(|| {
        MethodCallFailed::from(RuntimeError::IllegalArgumentException {
            message: "tclass is not a regular class mirror".to_string(),
        })
    })?;

    // JIT round 21 wave 1, lane updaters (`CRATONVM_FIELD_UPDATER_JDK_RULES`,
    // default ON): JDK 25 asks `tclass.getDeclaredField(fieldName)` -- the
    // class's OWN fields -- so a field a superclass declares is
    // `NoSuchFieldException`, wrapped in `RuntimeException` with the cause
    // kept. `=0` restores the superclass walk and the message-only wrapper.
    let jdk_rules = field_updater_jdk_rules();
    let mut found: Option<cratonvm_native_api::FieldMetadata> = None;
    let mut cursor = Some(tclass_id);
    while let Some(cid) = cursor {
        let fields = ctx.declared_fields(cid);
        if let Some(meta) = fields.into_iter().find(|m| m.name == field_name) {
            found = Some(meta);
            break;
        }
        cursor = if jdk_rules {
            None
        } else {
            ctx.superclass_of(cid)
        };
    }
    let Some(meta) = found else {
        tracing::warn!(
            tclass = ?ctx.class_name_of_id(tclass_id),
            field = %field_name,
            "T19.H5: newUpdater field not found"
        );
        return Err(if jdk_rules {
            new_updater_refusal_caused_by(ctx, "java/lang/NoSuchFieldException", field_name)
        } else {
            new_updater_refusal(
                ctx,
                &format!("java.lang.NoSuchFieldException: {field_name}"),
            )
        });
    };

    // JIT round 21 wave 1, lane updaters: still inside the JDK's `try`,
    // `ReflectUtil.ensureMemberAccess(caller, tclass, null, modifiers)`; a
    // refusal is `RuntimeException(IllegalAccessException(..))`. The caller
    // is kept for the `cclass` rule below.
    let modifiers = i32::from(meta.access_flags);
    let caller = if jdk_rules {
        match crate::lang_class::field_updater_member_access(ctx, tclass_id, modifiers) {
            Ok(caller) => caller,
            Err(text) => {
                return Err(new_updater_refusal_caused_by(
                    ctx,
                    "java/lang/IllegalAccessException",
                    &text,
                ))
            }
        }
    } else {
        None
    };

    // r21 wave 7, lane updaters (page `r21-review-w45-vm-findings-20261019.md`
    // item 2): a null `vclass` gets past the `try` -- a missing field or an
    // access refusal above wins -- and is then `null != fieldClass`, the bare
    // CCE, whatever the field's type.
    if vclass_null {
        return Err(RuntimeError::ClassCastException {
            message: String::new(),
        }
        .into());
    }

    // THE ORDER IS THE ORACLE'S, and it is measured rather than assumed
    // (`AtomicFamilySweep.java`): a field that fails two checks at
    // once names the one HotSpot reaches first.
    //
    //   non-volatile LONG on an int updater -> "Must be integer type"
    //   static non-volatile INT             -> "Must be volatile type"
    //   static volatile INT                 -> IllegalArgumentException, no message
    //
    // So: TYPE, then VOLATILE, then static. This file used to run static ->
    // volatile -> type and answer in its own words, so a static non-volatile
    // field named the static check where the oracle names the volatile one.

    // 1. TYPE. The REFERENCE variant on a non-reference field leaves by
    //    ClassCastException instead -- the JDK compares the field's declared
    //    Class against `vclass`, and a primitive can never equal a reference
    //    one, so it never reaches the descriptor check.
    if vclass_mirror.is_some() && !descriptor_is_reference(&meta.descriptor) {
        // r21 review item 4 (lane updaters): JDK 25 asks `vclass != fieldClass`
        // FIRST, then `vclass.isPrimitive()`. A primitive field whose type IS
        // the primitive `vclass` (`newUpdater(T.class, int.class, "i")`) passes
        // the identity test and is `IllegalArgumentException("Must be
        // reference type")`; any other pair is the message-less CCE.
        if jdk_rules {
            if let (Some(vmirror), Some(prim)) =
                (vclass_mirror, primitive_type_name(&meta.descriptor))
            {
                // By the mirror's own primitive flag and name (a primitive
                // mirror is canonical on the VM; asked without minting one).
                if crate::lang_class::mirror_is_primitive(&*ctx, vmirror)
                    && crate::lang_class::mirror_class_name(&*ctx, vmirror).as_deref() == Some(prim)
                {
                    return Err(RuntimeError::IllegalArgumentException {
                        message: "Must be reference type".to_string(),
                    }
                    .into());
                }
            }
        }
        return Err(RuntimeError::ClassCastException {
            // No message: HotSpot's is null here (measured).
            message: String::new(),
        }
        .into());
    }
    // JIT round 21 wave 1, lane updaters: JDK 25 `if (vclass != fieldClass)
    // throw new ClassCastException()` -- identity, before the volatile check
    // (`java.lang.Object` fields are no exception), and an int / long updater
    // takes exactly `int` / `long` (`field.getType() != int.class`).
    if jdk_rules {
        if let Some(vmirror) = vclass_mirror {
            // r21 review item 4 (lane updaters): IDENTITY, as JDK 25's
            // `vclass != field.getType()`. The field's type is resolved as
            // its DECLARING class resolves it (wave 7, below); only when the
            // resolver does not know it is the name compared. A primitive
            // `vclass` on a reference field (`int.class` has no class id here)
            // is the CCE -- it used to fall to the old path's IAE "vclass is
            // not a regular class mirror". Any other unresolvable mirror (a
            // descriptor-backed array stand-in) keeps the old path.
            //
            // r21 wave 7, lane updaters (page
            // `r21-review-w45-vm-findings-20261019.md` item 1): the type is
            // what the DECLARING class's constant pool resolves the name to,
            // its own loader as initiating loader -- the class it defined, a
            // direct supertype, or a name it already initiated (delegated to
            // any loader, a sibling included) -- read through the resolver's
            // no-loading door (`class_id_seen_by_class_without_loading`,
            // `constants.rs` `class_resolved_without_loading`). It used to
            // probe the loader's table and then its PARENT chain, which under
            // a child-first / sibling-delegating loader named a same-named
            // class the declaring class never sees and threw a false CCE.
            // Nothing is loaded here (a `loadClass` would be a GC point with
            // `tclass` / `vclass` unpinned): when the resolver does not know
            // yet, the name comparison below answers, as before.
            let field_type = field_type_internal_name(&meta.descriptor);
            let differs = match mirror_class_id(ctx, vmirror) {
                Some(vid) => {
                    let resolved = field_type.and_then(|name| {
                        ctx.class_id_seen_by_class_without_loading(name, meta.declaring_class_id)
                    });
                    match resolved {
                        Some(fid) => fid != vid,
                        None => ctx
                            .class_name_of_id(vid)
                            .is_some_and(|v| field_type != Some(v.as_str())),
                    }
                }
                None => crate::lang_class::mirror_is_primitive(ctx, vmirror),
            };
            if differs {
                return Err(RuntimeError::ClassCastException {
                    message: String::new(),
                }
                .into());
            }
        } else if !matches!(meta.descriptor.as_str(), "I" | "J") {
            return Err(RuntimeError::IllegalArgumentException {
                message: type_message.to_string(),
            }
            .into());
        }
    }
    let tag = expected(&meta.descriptor).ok_or_else(|| {
        tracing::warn!(
            field = %field_name,
            descriptor = %meta.descriptor,
            "T19.H5: newUpdater descriptor incompatible with updater variant"
        );
        MethodCallFailed::from(RuntimeError::IllegalArgumentException {
            message: type_message.to_string(),
        })
    })?;

    // 2. VOLATILE. ACC_VOLATILE = 0x0040. MEASURED (`probes/AtomicUpdaterSweep
    //    .java`, compatible mode): `newUpdater(Holder.class, "plainInt")` on a
    //    NON-volatile field answered `no-throw` where HotSpot throws.
    //
    //    This is the one validation whose absence is silently unsafe rather
    //    than merely wrong-typed: the whole contract of a field updater is that
    //    the field is volatile, and an updater handed a plain field gives every
    //    caller ordinary non-atomic reads and writes while looking exactly like
    //    an atomic one.
    if (meta.access_flags & 0x0040) == 0 {
        return Err(RuntimeError::IllegalArgumentException {
            message: "Must be volatile type".to_string(),
        }
        .into());
    }

    // 3. STATIC / FINAL. ACC_STATIC = 0x0008, ACC_FINAL = 0x0010. HotSpot
    //    reaches these through `Unsafe.objectFieldOffset`, which raises
    //    IllegalArgumentException with NO message.
    if meta.is_static || (meta.access_flags & 0x0008) != 0 {
        return Err(RuntimeError::IllegalArgumentException {
            message: String::new(),
        }
        .into());
    }
    if (meta.access_flags & 0x0010) != 0 {
        return Err(RuntimeError::IllegalArgumentException {
            message: String::new(),
        }
        .into());
    }

    // Reference variant: the JDK's reflective `newUpdater` performs a
    // type-equality check between the field's declared type and the
    // `vclass` argument, and throws `ClassCastException` on mismatch.
    let vclass_id = if let Some(vmirror) = vclass_mirror {
        let vid = mirror_class_id(ctx, vmirror).ok_or_else(|| {
            MethodCallFailed::from(RuntimeError::IllegalArgumentException {
                message: "vclass is not a regular class mirror".to_string(),
            })
        })?;
        if let Some(field_internal) = ref_descriptor_to_internal_name(&meta.descriptor) {
            // Tolerate same-class match exactly.  Cross-class assignment
            // is only legal if v == declared (per ARFU spec), unless the
            // declared type is `java.lang.Object` (which accepts any).
            let v_name = ctx.class_name_of_id(vid).unwrap_or_default();
            if v_name != field_internal && field_internal != "java/lang/Object" {
                tracing::warn!(
                    field = %field_name,
                    declared = %field_internal,
                    vclass = %v_name,
                    "T19.H5: newUpdater type mismatch (CCE)"
                );
                return Err(RuntimeError::ClassCastException {
                    // No message: HotSpot's is null here (measured). The
                    // `tracing::warn!` above keeps the diagnosis, which is
                    // where a VM-side detail belongs -- not in a message the
                    // application compares.
                    message: String::new(),
                }
                .into());
            }
        }
        vid.as_u32() as i32
    } else {
        0
    };

    // Allocate the synthetic impl, populate slots.
    let impl_obj = alloc_impl(ctx, impl_class)?;
    ctx.set_field(
        impl_obj,
        FU_SLOT_TCLASS_ID,
        Value::Int(tclass_id.as_u32() as i32),
    );
    ctx.set_field(
        impl_obj,
        FU_SLOT_FIELD_INDEX,
        Value::Int(meta.slot_index as i32),
    );
    ctx.set_field(impl_obj, FU_SLOT_DESC_TAG, Value::Int(tag));
    ctx.set_field(impl_obj, FU_SLOT_VCLASS_ID, Value::Int(vclass_id));
    // JIT round 21 wave 1, lane updaters: JDK 25 `this.cclass =
    // (Modifier.isProtected(modifiers) && tclass.isAssignableFrom(caller) &&
    // !isSamePackage(tclass, caller)) ? caller : tclass`.
    // Always written (`tclass` when no caller resolved, or under `=0`), so
    // the word never reads as a default `0` naming some other class.
    let mut cclass = tclass_id;
    if let Some(caller) = caller {
        if (modifiers & 0x0004) != 0
            && caller != tclass_id
            && ctx.is_subclass(caller, tclass_id)
            && !crate::lang_class::field_updater_same_package(ctx, tclass_id, caller)
        {
            cclass = caller;
        }
    }
    // Cast: a class id as the impl's int word, as `FU_SLOT_TCLASS_ID`.
    ctx.set_field(impl_obj, FU_SLOT_CCLASS_ID, Value::Int(cclass.as_u32() as i32));

    if atomic_updater_diag_enabled() {
        tracing::warn!(
            field = %field_name,
            target_class = %ctx.class_name_of_id(tclass_id).unwrap_or_default(),
            field_slot = meta.slot_index,
            descriptor = %meta.descriptor,
            impl_class = %object_class_name(ctx, impl_obj),
            impl_slots = ctx.object_num_fields(impl_obj),
            "T19.H5: AtomicFieldUpdater impl allocated"
        );
    }

    Ok(Some(Value::Object(Some(impl_obj))))
}

// ---------------------------------------------------------------------------
// newUpdater natives
// ---------------------------------------------------------------------------

/// The JDK's `newUpdater` refusals are wrapped in a plain `RuntimeException`.
///
/// All three factories have the same body shape:
///
/// ```java
/// try { field = tclass.getDeclaredField(fieldName); ... }
/// catch (Exception ex) { throw new RuntimeException(ex); }
/// ```
///
/// so a null `tclass`, a null field name and a missing field all leave as
/// `java.lang.RuntimeException` — not as the NPE or IAE that caused them.
/// MEASURED against HotSpot 25.0.3+9 (`probes/AtomicUpdaterSweep.java`):
///
/// ```text
/// newUpdater(Holder.class, "nope")   HotSpot RuntimeException  CratonVM IllegalArgumentException
/// newUpdater(null, "i")             HotSpot RuntimeException  CratonVM NullPointerException
/// newUpdater(Holder.class, null)    HotSpot RuntimeException  CratonVM NullPointerException
/// ```
///
/// Why it is worth correcting even though IAE and NPE are themselves
/// `RuntimeException`s, so a `catch (RuntimeException)` is unaffected: the
/// difference runs the OTHER way. Application code that catches
/// `IllegalArgumentException` around a `newUpdater` call — a reasonable thing to
/// write — catches this VM's refusal and does NOT catch HotSpot's, so a
/// recovery path that never runs on the reference VM runs here.
fn new_updater_refusal(ctx: &mut dyn NativeContext, msg: &str) -> MethodCallFailed {
    crate::phases_early::throw_jca_exc(ctx, "java/lang/RuntimeException", msg)
}

/// `CRATONVM_FIELD_UPDATER_JDK_RULES=0` (JIT round 21 wave 1, lane updaters;
/// page `r21-compat-field-updater-factory-skips-jdk-access-rules-20261018.md`)
/// restores the `--compatible` factories' old rules: the field looked up on
/// the superclass chain, no member-access check, no protected `cclass`, any
/// int-shaped field for an int updater, a `java.lang.Object` field for any
/// `vclass`, and refusals wrapped without their cause. Default ON. Read once
/// per `newUpdater` (cold).
fn field_updater_jdk_rules() -> bool {
    true
}

/// `throw new RuntimeException(new <cause_class>(cause_msg))`, the JDK
/// factories' `catch (Exception ex) { throw new RuntimeException(ex); }`: the
/// message is `cause.toString()` and `getCause()` is the cause. Falls back to
/// [`new_updater_refusal`]'s message-only form when either object cannot be
/// built.
fn new_updater_refusal_caused_by(
    ctx: &mut dyn NativeContext,
    cause_class: &str,
    cause_msg: &str,
) -> MethodCallFailed {
    let fallback = format!("{}: {cause_msg}", cause_class.replace('/', "."));
    // Computed detail text: fresh, never pooled (JIT round 22 wave 3, lane
    // review; `r13_misc10_create_string_ratchet`).
    let detail = ctx.create_string_uninterned(cause_msg);
    let cause = match ctx.new_object_initialized(
        cause_class,
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(detail))],
    ) {
        Ok(Some(Value::Object(Some(cause)))) => cause,
        _ => return new_updater_refusal(ctx, &fallback),
    };
    // The second allocation may move the cause.
    let pin = ctx.pin_native_root(cause);
    let cause = ctx.read_native_pin(pin, cause);
    ctx.unpin_native_roots(pin);
    match cratonvm_native_api::throwable_builder::construct_new(
        ctx,
        "java/lang/RuntimeException",
        "(Ljava/lang/Throwable;)V",
        &[Value::Object(Some(cause))],
    ) {
        Ok(Some(exc)) => MethodCallFailed::ExceptionThrown(exc),
        Err(thrown) => thrown,
        Ok(None) => new_updater_refusal(ctx, &fallback),
    }
}

/// A null `fieldName` (r21 wave 7, lane updaters; page
/// `r21-review-w45-vm-findings-20261019.md` item 2). JDK 25's
/// `Class.getDeclaredField` opens with `Objects.requireNonNull(name)`, a
/// message-less `new NullPointerException()` (no helpful text: it is thrown
/// explicitly, not by the VM), which the factories' `catch (Exception)` wraps:
/// `RuntimeException`, message `java.lang.NullPointerException`, cause the
/// NPE. The same for all three factories. `jdk_rules` off
/// (`CRATONVM_FIELD_UPDATER_JDK_RULES=0`) keeps the old message-only
/// `java.lang.NullPointerException: fieldName`.
fn null_field_name_refusal(ctx: &mut dyn NativeContext, jdk_rules: bool) -> MethodCallFailed {
    if !jdk_rules {
        return new_updater_refusal(ctx, "java.lang.NullPointerException: fieldName");
    }
    let cause = match cratonvm_native_api::throwable_builder::construct_new(
        ctx,
        "java/lang/NullPointerException",
        "()V",
        &[],
    ) {
        Ok(Some(cause)) => cause,
        Err(thrown) => return thrown,
        Ok(None) => return new_updater_refusal(ctx, NULL_FIELD_NAME_TEXT),
    };
    match cratonvm_native_api::throwable_builder::construct_new(
        ctx,
        "java/lang/RuntimeException",
        "(Ljava/lang/Throwable;)V",
        &[Value::Object(Some(cause))],
    ) {
        Ok(Some(exc)) => MethodCallFailed::ExceptionThrown(exc),
        Err(thrown) => thrown,
        Ok(None) => new_updater_refusal(ctx, NULL_FIELD_NAME_TEXT),
    }
}

/// HotSpot 25's `RuntimeException` message for a null `fieldName`: the
/// wrapped message-less NPE's `toString()`.
const NULL_FIELD_NAME_TEXT: &str = "java.lang.NullPointerException";

/// The internal class name `Field.getType()` names for a field descriptor:
/// `Lp/C;` -> `p/C`, an array descriptor itself (`[I`, `[Lp/C;`), `None` for
/// a primitive.
fn field_type_internal_name(desc: &str) -> Option<&str> {
    if desc.starts_with('[') {
        Some(desc)
    } else if desc.len() > 2 && desc.starts_with('L') && desc.ends_with(';') {
        Some(&desc[1..desc.len() - 1])
    } else {
        None
    }
}

fn native_arfu_new_updater(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // `new_updater_refusal`, not `arg_obj_or_npe`: see that helper for the
    // measured shape. The JDK reaches these through `getDeclaredField` inside
    // a `catch (Exception) -> RuntimeException`, so the NPE never escapes.
    let Some(tclass) = arg_obj(args, 0) else {
        return Err(new_updater_refusal(
            ctx,
            // Transcribed from the oracle: HotSpot's helpful NPE names the
            // call `newUpdater` is about to make. ONE LINE deliberately -- a
            // Rust `\` continuation inside a literal has already baked source
            // indentation into a transcribed message once in this lane.
            NULL_TCLASS_NPE,
        ));
    };
    // r21 wave 7, lane updaters (page `r21-review-w45-vm-findings-20261019.md`
    // item 2): under `CRATONVM_FIELD_UPDATER_JDK_RULES` a null `vclass` is
    // not refused here -- JDK 25 never reads it inside the `try`, so a null
    // `fieldName` (and a missing or inaccessible field) is answered first and
    // the null `vclass` is `build_updater`'s bare CCE. `=0` keeps the old
    // `vclass` refusal ahead of the name.
    let jdk_rules = field_updater_jdk_rules();
    let vclass = arg_obj(args, 1);
    if vclass.is_none() && !jdk_rules {
        return Err(new_updater_refusal(
            ctx,
            "java.lang.NullPointerException: vclass",
        ));
    }
    let Some(name_obj) = arg_obj(args, 2) else {
        return Err(null_field_name_refusal(ctx, jdk_rules));
    };
    let name = ctx.read_string(name_obj).unwrap_or_default();
    tracing::info!(
        field = %name,
        "AtomicReferenceFieldUpdater.newUpdater native dispatched"
    );
    build_updater(
        ctx,
        tclass,
        &name,
        |d| {
            if descriptor_is_reference(d) {
                Some(DESC_TAG_REF)
            } else {
                None
            }
        },
        CLS_REF_FIELD_UPDATER_IMPL,
        vclass,
        vclass.is_none(),
        "",
    )
}

fn native_aifu_new_updater(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // See `new_updater_refusal`.
    let Some(tclass) = arg_obj(args, 0) else {
        return Err(new_updater_refusal(
            ctx,
            // Transcribed from the oracle: HotSpot's helpful NPE names the
            // call `newUpdater` is about to make. ONE LINE deliberately -- a
            // Rust `\` continuation inside a literal has already baked source
            // indentation into a transcribed message once in this lane.
            NULL_TCLASS_NPE,
        ));
    };
    let Some(name_obj) = arg_obj(args, 1) else {
        return Err(null_field_name_refusal(ctx, field_updater_jdk_rules()));
    };
    let name = ctx.read_string(name_obj).unwrap_or_default();
    build_updater(
        ctx,
        tclass,
        &name,
        descriptor_tag_for_int_variant,
        CLS_INT_FIELD_UPDATER_IMPL,
        None,
        false,
        "Must be integer type",
    )
}

fn native_alfu_new_updater(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // See `new_updater_refusal`.
    let Some(tclass) = arg_obj(args, 0) else {
        return Err(new_updater_refusal(
            ctx,
            // Transcribed from the oracle: HotSpot's helpful NPE names the
            // call `newUpdater` is about to make. ONE LINE deliberately -- a
            // Rust `\` continuation inside a literal has already baked source
            // indentation into a transcribed message once in this lane.
            NULL_TCLASS_NPE,
        ));
    };
    let Some(name_obj) = arg_obj(args, 1) else {
        return Err(null_field_name_refusal(ctx, field_updater_jdk_rules()));
    };
    let name = ctx.read_string(name_obj).unwrap_or_default();
    build_updater(
        ctx,
        tclass,
        &name,
        descriptor_tag_for_long_variant,
        CLS_LONG_FIELD_UPDATER_IMPL,
        None,
        false,
        "Must be long type",
    )
}

// ---------------------------------------------------------------------------
// Helpers for accessor implementations
// ---------------------------------------------------------------------------

fn impl_slot(ctx: &dyn NativeContext, impl_obj: ObjectRef) -> Option<usize> {
    match ctx.get_field(impl_obj, FU_SLOT_FIELD_INDEX) {
        Value::Int(v) if v >= 0 => Some(v as usize),
        _ => None,
    }
}

fn atomic_updater_diag_enabled() -> bool {
    crate::nbflags().dbg_atomic_updater
}

fn object_class_name(ctx: &dyn NativeContext, obj: ObjectRef) -> String {
    let cid = ctx.class_id_of_object(obj);
    ctx.class_name_of_id(cid)
        .unwrap_or_else(|| format!("<class {:?}>", cid))
}

fn describe_value(ctx: &dyn NativeContext, value: Value) -> String {
    match value {
        Value::Object(Some(obj)) => {
            format!("Object({})", object_class_name(ctx, obj))
        }
        Value::Object(None) => "Object(null)".to_string(),
        Value::Int(v) => format!("Int({v})"),
        Value::Long(v) => format!("Long({v})"),
        Value::Float(v) => format!("Float({v})"),
        Value::Double(v) => format!("Double({v})"),
        Value::ReturnAddress(v) => format!("ReturnAddress({v})"),
        Value::Uninitialized => "Uninitialized".to_string(),
    }
}

/// The target object, checked against the updater's `tclass`.
///
/// The JDK's accessors open with `if (!tclass.isInstance(obj)) throw new
/// ClassCastException()` — an `isInstance` that is FALSE for null, so a null
/// target leaves by the same door as a wrong-typed one. Both rows were wrong
/// here (`probes/AtomicUpdaterSweep.java`, compatible mode):
///
/// ```text
/// u.get(null)                       HotSpot ClassCastException  CratonVM NPE
/// rawUpdater.get(new Object())      HotSpot ClassCastException  CratonVM no-throw
/// ```
///
/// The second is the dangerous one. An `AtomicIntegerFieldUpdater<Holder>` cast
/// to a raw type and applied to some other class read and wrote **slot N of an
/// unrelated object** — whatever happens to live at the offset `Holder.i`
/// occupies. Generics are erased, so the cast that makes this reachable is one
/// an application can perform by accident, and the JDK's runtime check is the
/// only thing standing between it and a silent cross-object write.
///
/// `tclass_id` comes from the updater's own `FU_SLOT_TCLASS_ID`; a slot that
/// does not hold a class id (an updater minted before this field was populated,
/// or a foreign receiver) skips the check rather than inventing a failure.
fn require_target(
    ctx: &mut dyn NativeContext,
    updater: ObjectRef,
    args: &[Value],
    idx: usize,
) -> Result<ObjectRef, MethodCallFailed> {
    // JIT round 21 wave 1, lane updaters: the JDK's `cclass` when it is not
    // `tclass` (a protected field reached from a foreign-package subclass).
    let tclass_word = ctx.get_field(updater, FU_SLOT_TCLASS_ID);
    let cclass = match (tclass_word, ctx.get_field(updater, FU_SLOT_CCLASS_ID)) {
        (Value::Int(t), Value::Int(c)) if c >= 0 && t >= 0 && c != t => {
            // Cast: non-negative class ids stored by `build_updater`.
            Some((
                cratonvm_types::ClassId::new(c as u32),
                cratonvm_types::ClassId::new(t as u32),
            ))
        }
        _ => None,
    };
    let Some(target) = (match args.get(idx) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    }) else {
        if cclass.is_some() {
            // `throwAccessCheckException(null)` with `cclass != tclass`
            // builds its message from `obj.getClass()`: HotSpot's helpful NPE.
            return Err(RuntimeError::NullPointerException {
                message: Some(
                    "Cannot invoke \"Object.getClass()\" because \"obj\" is null".to_string(),
                ),
            }
            .into());
        }
        // Null target: `isInstance(null)` is false, so this is a
        // ClassCastException, not the NullPointerException `arg_obj_or_npe`
        // would raise.
        // JDK 25 `throwAccessCheckException` / `throwCCE`: `new
        // ClassCastException()`, a null message (an empty one is the "no
        // message" marker). Wave 100 L7: this carried a text of its own.
        return Err(RuntimeError::ClassCastException {
            message: String::new(),
        }
        .into());
    };
    if let Some((cclass, tclass)) = cclass {
        // `cclass` is a subclass of `tclass`, so passing it passes the
        // `tclass` test below too.
        let is_array = ctx.heap_kind_of(target) == cratonvm_types::ObjectKind::Array;
        let actual = ctx.class_id_of_object(target);
        if is_array || (actual != cclass && !ctx.is_subclass(actual, cclass)) {
            // The target's name FIRST: `java_class_get_name` runs Java
            // (`Class.getName`, and may mint a mirror), a GC point that may
            // move `target`, which is not pinned here (r21 review w45-vm).
            let instance_name = java_object_class_name(ctx, target);
            let text = format!(
                "Class {} can not access a protected member of class {} using an instance of {}",
                java_class_get_name(ctx, cclass),
                java_class_get_name(ctx, tclass),
                instance_name,
            );
            return Err(new_updater_refusal_caused_by(
                ctx,
                "java/lang/IllegalAccessException",
                &text,
            ));
        }
        return Ok(target);
    }
    if let Value::Int(tclass_raw) = tclass_word {
        if tclass_raw >= 0 {
            let tclass = cratonvm_types::ClassId::new(tclass_raw as u32);
            let actual = ctx.class_id_of_object(target);
            // An array's header carries its COMPONENT's class id, so an `H[]`
            // passed the id test for an `H` updater and had `H`'s slot written
            // into it. No updater's class is an array's supertype (`tclass`
            // declares the volatile field, and `Object` declares none).
            let is_array = ctx.heap_kind_of(target) == cratonvm_types::ObjectKind::Array;
            if is_array || (actual != tclass && !ctx.is_subclass(actual, tclass)) {
                // `new ClassCastException()`: no message, as above (wave 100
                // L7; this said "<class> is not an instance of <tclass>").
                return Err(RuntimeError::ClassCastException {
                    message: String::new(),
                }
                .into());
            }
        }
    }
    Ok(target)
}

/// r21 review item 6 (lane updaters): `Class.getName()` of `class_id`, asked
/// of its mirror (a hidden class's name keeps its `/0x..` tail, which the
/// dotted internal name would not spell); the dotted internal name when the
/// call fails.
fn java_class_get_name(ctx: &mut dyn NativeContext, class_id: cratonvm_types::ClassId) -> String {
    let fallback = java_class_name(&*ctx, class_id);
    let mirror = ctx.get_class_mirror(class_id);
    match ctx.invoke_virtual(mirror, "getName", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(name)))) => ctx.read_string(name).unwrap_or(fallback),
        _ => fallback,
    }
}

/// The Java name of a primitive field descriptor (`I` -> `int`), `None` for a
/// reference.
fn primitive_type_name(desc: &str) -> Option<&'static str> {
    Some(match desc {
        "Z" => "boolean",
        "B" => "byte",
        "S" => "short",
        "C" => "char",
        "I" => "int",
        "J" => "long",
        "F" => "float",
        "D" => "double",
        _ => return None,
    })
}

/// `Class.getName()` of a (non-array) class id: the internal name dotted.
fn java_class_name(ctx: &dyn NativeContext, class_id: cratonvm_types::ClassId) -> String {
    ctx.class_name_of_id(class_id)
        .unwrap_or_default()
        .class_text(&*ctx)
}

/// `obj.getClass().getName()`, asked of the object itself (an array's header
/// names its component, and a hidden class's name is not its internal one);
/// the dotted header name when the calls fail.
fn java_object_class_name(ctx: &mut dyn NativeContext, obj: ObjectRef) -> String {
    let fallback = java_class_name(&*ctx, ctx.class_id_of_object(obj));
    let Ok(Some(Value::Object(Some(mirror)))) =
        ctx.invoke_virtual(obj, "getClass", "()Ljava/lang/Class;", &[])
    else {
        return fallback;
    };
    match ctx.invoke_virtual(mirror, "getName", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(name)))) => ctx.read_string(name).unwrap_or(fallback),
        _ => fallback,
    }
}

/// The new value of a reference updater's store, checked against its
/// `vclass` (JIT round 21 wave 1, lane updaters).
///
/// JDK 25 `AtomicReferenceFieldUpdaterImpl.set` / `lazySet` /
/// `compareAndSet` / `weakCompareAndSet` / `getAndSet` run `valueCheck(v)`
/// after `accessCheck(obj)`: `if (v != null && !vclass.isInstance(v))
/// throwCCE()`, a `ClassCastException` with a null message; the base class's
/// `getAndUpdate` / `updateAndGet` reach it through `compareAndSet`. These
/// natives recorded `vclass` (`FU_SLOT_VCLASS_ID`) but never asked it, so a
/// raw-typed updater stored, say, a `String` into a field declared `Foo` --
/// a heap type confusion every later `getfield` + `checkcast`-free use of the
/// field trusts. An array value is an instance only of `Object`,
/// `Cloneable` and `Serializable` among the non-array classes `vclass` can be
/// here (`mirror_class_id` refuses an array mirror at `newUpdater`), and its
/// header id is its component's, so it is answered by name. A slot that holds
/// no class id (an int/long updater's `0`) skips the check.
fn require_value(
    ctx: &mut dyn NativeContext,
    updater: ObjectRef,
    value: Value,
) -> Result<(), MethodCallFailed> {
    let vclass_word = ctx.get_field(updater, FU_SLOT_VCLASS_ID);
    require_value_of(ctx, vclass_word, value)
}

/// [`require_value`] with the updater's `FU_SLOT_VCLASS_ID` word already
/// read (`arfu_update_with_operator` reads it once, before its operator runs
/// Java code that may move the updater).
fn require_value_of(
    ctx: &mut dyn NativeContext,
    vclass_word: Value,
    value: Value,
) -> Result<(), MethodCallFailed> {
    let Value::Object(Some(v)) = value else {
        return Ok(());
    };
    let Value::Int(raw) = vclass_word else {
        return Ok(());
    };
    if raw <= 0 {
        return Ok(());
    }
    // Cast: a non-negative class id stored by `newUpdater`.
    let vclass = cratonvm_types::ClassId::new(raw as u32);
    let ok = if ctx.heap_kind_of(v) == cratonvm_types::ObjectKind::Array {
        matches!(
            ctx.class_name_of_id(vclass).as_deref(),
            Some("java/lang/Object" | "java/lang/Cloneable" | "java/io/Serializable")
        )
    } else {
        let actual = ctx.class_id_of_object(v);
        // `Object.isInstance(v)` is true for every non-null `v`: answered by
        // name too (asked only when the id test fails), so a context whose
        // supertype walk does not reach `java/lang/Object` (a stand-in class,
        // the test mock) cannot refuse a legal store.
        actual == vclass
            || ctx.is_subclass(actual, vclass)
            || ctx.class_name_of_id(vclass).as_deref() == Some("java/lang/Object")
            // r21 review (rev-native): the bare id walk refuses instances of
            // the VM's synthetic carriers -- a `java.lang.reflect.Proxy`
            // (`Proxy$Instance`, its interfaces per instance), an annotation
            // proxy -- that implement `vclass` (`native_class_is_instance`'s
            // own notes). Asked only when the walk failed, through the rule
            // `Class.isInstance` itself answers by (no allocation: `vclass`'s
            // mirror was the `newUpdater` argument, so it exists).
            || {
                let mirror = ctx.get_class_mirror(vclass);
                matches!(
                    crate::lang_class::native_class_is_instance(
                        ctx,
                        &[Value::Object(Some(mirror)), Value::Object(Some(v))],
                    ),
                    Ok(Some(Value::Int(1)))
                )
            }
    };
    // `CRATONVM_ARFU_VALUE_CHECK=0` restores the unchecked store (the defect);
    // read only on a failing check, so the passing path never reads the
    // environment.
    if ok {
        Ok(())
    } else {
        // JDK 25 `throwCCE()`: `new ClassCastException()`, a null message.
        Err(RuntimeError::ClassCastException {
            message: String::new(),
        }
        .into())
    }
}

fn pinned_object_value(ctx: &mut dyn NativeContext, value: Value) -> Option<(usize, ObjectRef)> {
    match value {
        Value::Object(Some(obj)) => Some((ctx.pin_native_root(obj), obj)),
        _ => None,
    }
}

fn read_pinned_object_value(
    ctx: &dyn NativeContext,
    pin: Option<(usize, ObjectRef)>,
    fallback: Value,
) -> Value {
    match pin {
        Some((handle, obj)) => Value::Object(Some(ctx.read_native_pin(handle, obj))),
        None => fallback,
    }
}

fn arfu_update_with_operator(
    ctx: &mut dyn NativeContext,
    target: ObjectRef,
    slot: usize,
    op: ObjectRef,
    return_new: bool,
    // JIT round 21 wave 1, lane updaters: the updater's `FU_SLOT_VCLASS_ID`
    // word, for the `valueCheck` the JDK's `compareAndSet` runs on `next`.
    vclass_word: Value,
) -> MethodCallResult {
    let target_pin = ctx.pin_native_root(target);
    let op_pin = ctx.pin_native_root(op);
    let mut target_cur = target;
    let mut op_cur = op;
    loop {
        target_cur = ctx.read_native_pin(target_pin, target_cur);
        op_cur = ctx.read_native_pin(op_pin, op_cur);
        let prev = ctx.get_field_volatile(target_cur, slot);
        let prev_pin = pinned_object_value(ctx, prev);
        let arg = read_pinned_object_value(ctx, prev_pin, prev);
        let new_val = match ctx.invoke_virtual(
            op_cur,
            "apply",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            &[arg],
        ) {
            Ok(v) => v.unwrap_or(Value::Object(None)),
            Err(e) => {
                ctx.unpin_native_roots(target_pin);
                return Err(e);
            }
        };
        // JDK 25: `next = updateFunction.apply(prev)` then `compareAndSet(obj,
        // prev, next)`, whose `valueCheck(next)` throws before any write.
        if let Err(e) = require_value_of(ctx, vclass_word, new_val) {
            if let Some((handle, _)) = prev_pin {
                ctx.unpin_native_roots(handle);
            }
            ctx.unpin_native_roots(target_pin);
            return Err(e);
        }
        target_cur = ctx.read_native_pin(target_pin, target_cur);
        let expected = read_pinned_object_value(ctx, prev_pin, prev);
        if let Some((handle, _)) = prev_pin {
            ctx.unpin_native_roots(handle);
        }
        if ctx.compare_and_swap_field(target_cur, slot, expected, new_val) {
            ctx.unpin_native_roots(target_pin);
            return Ok(Some(if return_new { new_val } else { expected }));
        }
    }
}

// ---------------------------------------------------------------------------
// Reference-variant accessors
// ---------------------------------------------------------------------------

fn native_arfu_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = arg_obj_or_npe(args, 0, "updater")?;
    let target = require_target(ctx, this, args, 1)?;
    let slot = impl_slot(ctx, this).unwrap_or(0);
    let value = ctx.get_field_volatile(target, slot);
    if atomic_updater_diag_enabled() {
        tracing::warn!(
            updater_class = %object_class_name(ctx, this),
            updater_slots = ctx.object_num_fields(this),
            slot,
            target_class = %object_class_name(ctx, target),
            target_slots = ctx.object_num_fields(target),
            value = %describe_value(ctx, value),
            "T19.H5: AtomicReferenceFieldUpdater.get"
        );
    }
    Ok(Some(value))
}

fn native_arfu_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = arg_obj_or_npe(args, 0, "updater")?;
    let target = require_target(ctx, this, args, 1)?;
    let new_val = arg_value(args, 2);
    require_value(ctx, this, new_val)?;
    let slot = impl_slot(ctx, this).unwrap_or(0);
    if atomic_updater_diag_enabled() {
        tracing::warn!(
            updater_class = %object_class_name(ctx, this),
            updater_slots = ctx.object_num_fields(this),
            slot,
            target_class = %object_class_name(ctx, target),
            target_slots = ctx.object_num_fields(target),
            value = %describe_value(ctx, new_val),
            "T19.H5: AtomicReferenceFieldUpdater.set"
        );
    }
    ctx.set_field_volatile(target, slot, new_val);
    Ok(None)
}

fn native_arfu_lazy_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // lazySet has weaker memory ordering on real HotSpot; on cratonvm we
    // bind it to the volatile path for correctness, accepting a
    // micro-perf cost rather than risk a re-ordering bug.
    native_arfu_set(ctx, args)
}

fn native_arfu_compare_and_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = arg_obj_or_npe(args, 0, "updater")?;
    let target = require_target(ctx, this, args, 1)?;
    let expected = arg_value(args, 2);
    let new_val = arg_value(args, 3);
    require_value(ctx, this, new_val)?;
    let slot = impl_slot(ctx, this).unwrap_or(0);
    let ok = ctx.compare_and_swap_field(target, slot, expected, new_val);
    Ok(Some(Value::Int(if ok { 1 } else { 0 })))
}

fn native_arfu_get_and_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = arg_obj_or_npe(args, 0, "updater")?;
    let target = require_target(ctx, this, args, 1)?;
    let new_val = arg_value(args, 2);
    require_value(ctx, this, new_val)?;
    let slot = impl_slot(ctx, this).unwrap_or(0);
    // Linearizable getAndSet: read+CAS until the swap commits. The CAS
    // primitive (`compare_and_swap_field`) holds the per-object CAS lock
    // across its own load+compare+store, so each iteration's
    // read-then-CAS pair is atomic against concurrent updaters.
    //
    // We must NOT fall through to a bare `set_field_volatile` after a
    // bounded number of tries: a non-atomic read-then-store loses the
    // update of any concurrent writer that committed between our read
    // and our store, and would return a "previous" value that was never
    // the one actually replaced — breaking getAndSet's contract. Real
    // HotSpot (`Unsafe.getAndSetReference`) loops until success, so we do
    // too. The CAS only fails when another writer made progress, which
    // guarantees system-wide progress (lock-free), not livelock.
    loop {
        let current = ctx.get_field_volatile(target, slot);
        if ctx.compare_and_swap_field(target, slot, current, new_val) {
            return Ok(Some(current));
        }
    }
}

fn native_arfu_get_and_update(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // getAndUpdate(T target, UnaryOperator<V> op).  We invoke the
    // operator via invoke_virtual on the Java Function to compute a new
    // value, then CAS until success.
    let this = arg_obj_or_npe(args, 0, "updater")?;
    let target = require_target(ctx, this, args, 1)?;
    let op = arg_obj_or_npe(args, 2, "op")?;
    let slot = impl_slot(ctx, this).unwrap_or(0);
    let vclass_word = ctx.get_field(this, FU_SLOT_VCLASS_ID);
    // The UnaryOperator is arbitrary Java code and cannot run inside the
    // CAS lock, so we recompute-and-CAS-retry until the swap commits —
    // exactly HotSpot's getAndUpdate. The loop must be unbounded: bailing
    // after a fixed retry count (the previous behaviour) abandoned the
    // update under contention, leaving the field unchanged but pretending
    // success — a lost update.
    arfu_update_with_operator(ctx, target, slot, op, false, vclass_word)
}

fn native_arfu_update_and_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = arg_obj_or_npe(args, 0, "updater")?;
    let target = require_target(ctx, this, args, 1)?;
    let op = arg_obj_or_npe(args, 2, "op")?;
    let slot = impl_slot(ctx, this).unwrap_or(0);
    let vclass_word = ctx.get_field(this, FU_SLOT_VCLASS_ID);
    // Unbounded recompute-and-CAS-retry (see native_arfu_get_and_update):
    // bailing after a fixed retry count would abandon the update and
    // return a value never actually stored.
    arfu_update_with_operator(ctx, target, slot, op, true, vclass_word)
}

// ---------------------------------------------------------------------------
// Int-variant accessors
// ---------------------------------------------------------------------------

fn native_aifu_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = arg_obj_or_npe(args, 0, "updater")?;
    let target = require_target(ctx, this, args, 1)?;
    let slot = impl_slot(ctx, this).unwrap_or(0);
    let v = match ctx.get_field_volatile(target, slot) {
        Value::Int(i) => i,
        Value::Long(l) => l as i32,
        _ => 0,
    };
    Ok(Some(Value::Int(v)))
}

fn native_aifu_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = arg_obj_or_npe(args, 0, "updater")?;
    let target = require_target(ctx, this, args, 1)?;
    let val = match arg_value(args, 2) {
        Value::Int(v) => v,
        Value::Long(v) => v as i32,
        _ => 0,
    };
    let slot = impl_slot(ctx, this).unwrap_or(0);
    ctx.set_field_volatile(target, slot, Value::Int(val));
    Ok(None)
}

fn native_aifu_compare_and_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = arg_obj_or_npe(args, 0, "updater")?;
    let target = require_target(ctx, this, args, 1)?;
    let expected = match arg_value(args, 2) {
        Value::Int(v) => v,
        _ => 0,
    };
    let new_val = match arg_value(args, 3) {
        Value::Int(v) => v,
        _ => 0,
    };
    let slot = impl_slot(ctx, this).unwrap_or(0);
    let ok = ctx.compare_and_swap_field(target, slot, Value::Int(expected), Value::Int(new_val));
    Ok(Some(Value::Int(if ok { 1 } else { 0 })))
}

fn native_aifu_get_and_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = arg_obj_or_npe(args, 0, "updater")?;
    let target = require_target(ctx, this, args, 1)?;
    let new_val = match arg_value(args, 2) {
        Value::Int(v) => v,
        _ => 0,
    };
    let slot = impl_slot(ctx, this).unwrap_or(0);
    // Linearizable getAndSet — read+CAS until commit, never a non-atomic
    // read-then-store fallthrough (which would lose a concurrent writer's
    // update and return a bogus "previous" value). See the reference
    // variant `native_arfu_get_and_set` for the full rationale.
    loop {
        let current = match ctx.get_field_volatile(target, slot) {
            Value::Int(v) => v,
            _ => 0,
        };
        if ctx.compare_and_swap_field(target, slot, Value::Int(current), Value::Int(new_val)) {
            return Ok(Some(Value::Int(current)));
        }
    }
}

fn native_aifu_get_and_add(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = arg_obj_or_npe(args, 0, "updater")?;
    let target = require_target(ctx, this, args, 1)?;
    let delta = match arg_value(args, 2) {
        Value::Int(v) => v,
        _ => 0,
    };
    let slot = impl_slot(ctx, this).unwrap_or(0);
    // Use the VM's dedicated atomic fetch-add primitive: a single
    // `LOCK XADD` (one trait dispatch, no CAS spin) returning the
    // *previous* value, exactly matching getAndAdd. The previous bounded
    // CAS loop silently returned 0 (and applied no update) after 1024
    // contended retries — a lost update. atomic_fetch_add_int loops until
    // commit and surfaces a field-type mismatch as a catchable exception.
    let prev = ctx.atomic_fetch_add_int(target, slot, delta)?;
    Ok(Some(Value::Int(prev)))
}

fn native_aifu_increment_and_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // incrementAndGet(target) ≡ getAndAdd(target, 1) + 1.
    // atomic_fetch_add_int returns the previous value via a single atomic
    // fetch-add; we add the delta back to yield the post-increment value.
    // Replaces a bounded CAS loop that returned 0 (no update applied)
    // under contention — a lost update.
    let this = arg_obj_or_npe(args, 0, "updater")?;
    let target = require_target(ctx, this, args, 1)?;
    let slot = impl_slot(ctx, this).unwrap_or(0);
    let prev = ctx.atomic_fetch_add_int(target, slot, 1)?;
    Ok(Some(Value::Int(prev.wrapping_add(1))))
}

fn native_aifu_decrement_and_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // decrementAndGet(target) ≡ getAndAdd(target, -1) - 1.
    let this = arg_obj_or_npe(args, 0, "updater")?;
    let target = require_target(ctx, this, args, 1)?;
    let slot = impl_slot(ctx, this).unwrap_or(0);
    let prev = ctx.atomic_fetch_add_int(target, slot, -1)?;
    Ok(Some(Value::Int(prev.wrapping_sub(1))))
}

// getAndIncrement/getAndDecrement/addAndGet — concrete methods on the abstract
// AtomicIntegerFieldUpdater base that the synthetic `$RustJvmImpl` subclass does
// not inherit, so they were NoSuchMethodError before (breaking Reactor, which
// uses them on field updaters for backpressure/state). getAnd* return the OLD
// value; addAndGet returns the NEW value. All three route through the VM's
// unbounded `atomic_fetch_add_int` primitive (a single LOCK XADD that loops
// until commit and surfaces a field-type mismatch as a catchable exception) —
// exactly like `getAndAdd`/`incrementAndGet`/`decrementAndGet` above. The
// previous bounded CAS loop silently returned a fabricated 0 (and applied no
// update) after 1024 contended retries — a lost update under contention.
fn native_aifu_get_and_increment(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = arg_obj_or_npe(args, 0, "updater")?;
    let target = require_target(ctx, this, args, 1)?;
    let slot = impl_slot(ctx, this).unwrap_or(0);
    // getAndIncrement returns the OLD value — exactly the previous value
    // reported by a fetch-add of +1.
    let prev = ctx.atomic_fetch_add_int(target, slot, 1)?;
    Ok(Some(Value::Int(prev)))
}

fn native_aifu_get_and_decrement(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = arg_obj_or_npe(args, 0, "updater")?;
    let target = require_target(ctx, this, args, 1)?;
    let slot = impl_slot(ctx, this).unwrap_or(0);
    // getAndDecrement returns the OLD value (previous value of fetch-add -1).
    let prev = ctx.atomic_fetch_add_int(target, slot, -1)?;
    Ok(Some(Value::Int(prev)))
}

fn native_aifu_add_and_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = arg_obj_or_npe(args, 0, "updater")?;
    let target = require_target(ctx, this, args, 1)?;
    let delta = match arg_value(args, 2) {
        Value::Int(v) => v,
        _ => 0,
    };
    let slot = impl_slot(ctx, this).unwrap_or(0);
    // addAndGet returns the NEW value: fetch-add reports the previous value,
    // so add the delta back to recover the post-update value.
    let prev = ctx.atomic_fetch_add_int(target, slot, delta)?;
    Ok(Some(Value::Int(prev.wrapping_add(delta))))
}

// ---------------------------------------------------------------------------
// Long-variant accessors
// ---------------------------------------------------------------------------

fn native_alfu_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = arg_obj_or_npe(args, 0, "updater")?;
    let target = require_target(ctx, this, args, 1)?;
    let slot = impl_slot(ctx, this).unwrap_or(0);
    let v = match ctx.get_field_volatile(target, slot) {
        Value::Long(l) => l,
        Value::Int(i) => i as i64,
        _ => 0,
    };
    Ok(Some(Value::Long(v)))}

fn native_alfu_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = arg_obj_or_npe(args, 0, "updater")?;
    let target = require_target(ctx, this, args, 1)?;
    let val = match arg_value(args, 2) {
        Value::Long(v) => v,
        Value::Int(v) => v as i64,
        _ => 0,
    };
    let slot = impl_slot(ctx, this).unwrap_or(0);
    ctx.set_field_volatile(target, slot, Value::Long(val));
    Ok(None)
}

fn native_alfu_compare_and_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = arg_obj_or_npe(args, 0, "updater")?;
    let target = require_target(ctx, this, args, 1)?;
    let expected = match arg_value(args, 2) {
        Value::Long(v) => v,
        _ => 0,
    };
    let new_val = match arg_value(args, 3) {
        Value::Long(v) => v,
        _ => 0,
    };
    let slot = impl_slot(ctx, this).unwrap_or(0);
    let ok = ctx.compare_and_swap_field(target, slot, Value::Long(expected), Value::Long(new_val));
    Ok(Some(Value::Int(if ok { 1 } else { 0 })))
}

fn native_alfu_get_and_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = arg_obj_or_npe(args, 0, "updater")?;
    let target = require_target(ctx, this, args, 1)?;
    let new_val = match arg_value(args, 2) {
        Value::Long(v) => v,
        _ => 0,
    };
    let slot = impl_slot(ctx, this).unwrap_or(0);
    // Linearizable getAndSet — read+CAS until commit, never a non-atomic
    // read-then-store fallthrough (which would lose a concurrent writer's
    // update and return a bogus "previous" value). See the reference
    // variant `native_arfu_get_and_set` for the full rationale.
    loop {
        let current = match ctx.get_field_volatile(target, slot) {
            Value::Long(v) => v,
            _ => 0,
        };
        if ctx.compare_and_swap_field(target, slot, Value::Long(current), Value::Long(new_val)) {
            return Ok(Some(Value::Long(current)));
        }
    }
}

fn native_alfu_get_and_add(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = arg_obj_or_npe(args, 0, "updater")?;
    let target = require_target(ctx, this, args, 1)?;
    let delta = match arg_value(args, 2) {
        Value::Long(v) => v,
        _ => 0,
    };
    let slot = impl_slot(ctx, this).unwrap_or(0);
    // Dedicated atomic fetch-add (LOCK XADD), returning the previous
    // value — matches getAndAdd and replaces a bounded CAS loop that
    // dropped the update (returned 0) under contention.
    let prev = ctx.atomic_fetch_add_long(target, slot, delta)?;
    Ok(Some(Value::Long(prev)))
}

// Long variants of getAndIncrement/getAndDecrement/addAndGet — see the int
// equivalents for rationale (synthetic `$RustJvmImpl` doesn't inherit the base's
// concrete methods). All three route through the unbounded
// `atomic_fetch_add_long` primitive, replacing a bounded CAS loop that returned
// a fabricated 0 (applying no update) on contended loop exhaustion.
fn native_alfu_get_and_increment(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = arg_obj_or_npe(args, 0, "updater")?;
    let target = require_target(ctx, this, args, 1)?;
    let slot = impl_slot(ctx, this).unwrap_or(0);
    // getAndIncrement returns the OLD value (previous value of fetch-add +1).
    let prev = ctx.atomic_fetch_add_long(target, slot, 1)?;
    Ok(Some(Value::Long(prev)))
}

fn native_alfu_get_and_decrement(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = arg_obj_or_npe(args, 0, "updater")?;
    let target = require_target(ctx, this, args, 1)?;
    let slot = impl_slot(ctx, this).unwrap_or(0);
    // getAndDecrement returns the OLD value (previous value of fetch-add -1).
    let prev = ctx.atomic_fetch_add_long(target, slot, -1)?;
    Ok(Some(Value::Long(prev)))
}

fn native_alfu_add_and_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = arg_obj_or_npe(args, 0, "updater")?;
    let target = require_target(ctx, this, args, 1)?;
    let delta = match arg_value(args, 2) {
        Value::Long(v) => v,
        _ => 0,
    };
    let slot = impl_slot(ctx, this).unwrap_or(0);
    // addAndGet returns the NEW value: fetch-add reports the previous value,
    // so add the delta back to recover the post-update value.
    let prev = ctx.atomic_fetch_add_long(target, slot, delta)?;
    Ok(Some(Value::Long(prev.wrapping_add(delta))))
}

// incrementAndGet/decrementAndGet — long equivalents of the int variants
// above. The synthetic `$RustJvmImpl` subclass doesn't inherit the abstract
// AtomicLongFieldUpdater base's concrete methods, and `register_alfu`
// previously registered only getAndIncrement/getAndDecrement/addAndGet — NOT
// incrementAndGet/decrementAndGet. kotlinx.coroutines' CoroutineScheduler
// calls `incrementAndGet(Object)J` (worker-id bump in createNewWorker), which
// hit a fatal NoSuchMethodError that ABENDed the VM. Both return the NEW value
// (fetch-add reports the previous, so we fold the delta back in).
fn native_alfu_increment_and_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // incrementAndGet(target) ≡ getAndAdd(target, 1) + 1.
    let this = arg_obj_or_npe(args, 0, "updater")?;
    let target = require_target(ctx, this, args, 1)?;
    let slot = impl_slot(ctx, this).unwrap_or(0);
    let prev = ctx.atomic_fetch_add_long(target, slot, 1)?;
    Ok(Some(Value::Long(prev.wrapping_add(1))))
}

fn native_alfu_decrement_and_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // decrementAndGet(target) ≡ getAndAdd(target, -1) - 1.
    let this = arg_obj_or_npe(args, 0, "updater")?;
    let target = require_target(ctx, this, args, 1)?;
    let slot = impl_slot(ctx, this).unwrap_or(0);
    let prev = ctx.atomic_fetch_add_long(target, slot, -1)?;
    Ok(Some(Value::Long(prev.wrapping_sub(1))))
}

// ---------------------------------------------------------------------------
// Registration entry point
// ---------------------------------------------------------------------------

/// T19.H5: register every `*FieldUpdater.newUpdater` factory + the
/// per-impl accessor surface.  Called from `register_essential_natives`
/// in `lib.rs`.
///
/// **Registers nothing under [`CompatibilityMode::JdkOnly`].** The full
/// argument is in the module header; the short form is that `newUpdater`
/// returns a `…FieldUpdater$RustJvmImpl` receiver no JDK image declares, strict
/// mode refuses to fabricate it, and the refusal lands as `NoClassDefFoundError`
/// in `java.sql.SQLException.<clinit>` — i.e. on the whole `java.sql` package.
///
/// The guard is here, at the single registrar, rather than at the two `lib.rs`
/// call sites (`register_essential_natives_with_shims`, which runs in **both**
/// modes, and `register_synthetic_overrides`, which is synthetic-only and
/// therefore never strict) so that a third call site cannot reintroduce the
/// rows. `NativeMethodRegistry::set_compatibility_mode` is called once in
/// `vm/src/vm/vm_init.rs` *above* the mode fork and before any `register_*`
/// pass, so `compatibility_mode()` is already correct when we are called.
///
/// It is a `return`, not a `set_category(SyntheticStub)`, deliberately. Both
/// would drop the rows in strict mode, but re-tagging changes the kind in
/// **Compatible** mode too, where these ~46 rows are `Bridge` today: it would
/// push `the native-registry census::synthetic_stub_count_does_not_regress`
/// (baseline 1263, `SLACK = 0`) over its frozen ceiling and move ~31 rows in
/// `scripts/baselines/jdk-only-kind-map-25-linux.tsv`. That ratchet's own
/// failure text says not to raise the baseline. Compatible mode must come out
/// of this change bit-identical, and with the `return` it does: same rows, same
/// kinds, same counts.
pub fn register_atomic_updater_natives(registry: &mut NativeMethodRegistry) {
    if registry.compatibility_mode() == cratonvm_types::compat::CompatibilityMode::JdkOnly {
        tracing::info!(
            "T19.H5: --real-jdk — NOT registering the Atomic{{Reference,Integer,Long}}FieldUpdater \
             natives; the real java.util.concurrent.atomic *FieldUpdaterImpl bytecode runs \
             instead (newUpdater would mint …FieldUpdater$RustJvmImpl, which no JDK image \
             declares and strict mode refuses — taking java.sql.SQLException.<clinit>, and \
             therefore the whole java.sql package, with it)"
        );
        return;
    }
    register_arfu(registry);
    register_aifu(registry);
    register_alfu(registry);
    tracing::info!(
        "T19.H5: registered AtomicReferenceFieldUpdater / AtomicIntegerFieldUpdater / AtomicLongFieldUpdater natives"
    );
}

fn register_arfu(r: &mut NativeMethodRegistry) {
    // Static factory: AtomicReferenceFieldUpdater.newUpdater(Class<U>, Class<V>, String).
    r.register(
        CLS_REF_FIELD_UPDATER,
        "newUpdater",
        "(Ljava/lang/Class;Ljava/lang/Class;Ljava/lang/String;)Ljava/util/concurrent/atomic/AtomicReferenceFieldUpdater;",
        native_arfu_new_updater, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    // Accessor methods on the synthetic impl.
    r.register(
        CLS_REF_FIELD_UPDATER_IMPL,
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_arfu_get, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        CLS_REF_FIELD_UPDATER_IMPL,
        "set",
        "(Ljava/lang/Object;Ljava/lang/Object;)V",
        native_arfu_set, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        CLS_REF_FIELD_UPDATER_IMPL,
        "lazySet",
        "(Ljava/lang/Object;Ljava/lang/Object;)V",
        native_arfu_lazy_set, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        CLS_REF_FIELD_UPDATER_IMPL,
        "compareAndSet",
        "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Z",
        native_arfu_compare_and_set, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        CLS_REF_FIELD_UPDATER_IMPL,
        "weakCompareAndSet",
        "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Z",
        native_arfu_compare_and_set, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        CLS_REF_FIELD_UPDATER_IMPL,
        "getAndSet",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_arfu_get_and_set, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        CLS_REF_FIELD_UPDATER_IMPL,
        "getAndUpdate",
        "(Ljava/lang/Object;Ljava/util/function/UnaryOperator;)Ljava/lang/Object;",
        native_arfu_get_and_update, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        CLS_REF_FIELD_UPDATER_IMPL,
        "updateAndGet",
        "(Ljava/lang/Object;Ljava/util/function/UnaryOperator;)Ljava/lang/Object;",
        native_arfu_update_and_get, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    // Also register accessors on the abstract base class so virtual
    // dispatch from generic code that doesn't see the impl class still
    // finds an implementation.
}

fn register_aifu(r: &mut NativeMethodRegistry) {
    r.register(
        CLS_INT_FIELD_UPDATER,
        "newUpdater",
        "(Ljava/lang/Class;Ljava/lang/String;)Ljava/util/concurrent/atomic/AtomicIntegerFieldUpdater;",
        native_aifu_new_updater, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        CLS_INT_FIELD_UPDATER_IMPL,
        "get",
        "(Ljava/lang/Object;)I",
        native_aifu_get, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        CLS_INT_FIELD_UPDATER_IMPL,
        "set",
        "(Ljava/lang/Object;I)V",
        native_aifu_set, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        CLS_INT_FIELD_UPDATER_IMPL,
        "lazySet",
        "(Ljava/lang/Object;I)V",
        native_aifu_set, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        CLS_INT_FIELD_UPDATER_IMPL,
        "compareAndSet",
        "(Ljava/lang/Object;II)Z",
        native_aifu_compare_and_set, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        CLS_INT_FIELD_UPDATER_IMPL,
        "weakCompareAndSet",
        "(Ljava/lang/Object;II)Z",
        native_aifu_compare_and_set, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        CLS_INT_FIELD_UPDATER_IMPL,
        "getAndSet",
        "(Ljava/lang/Object;I)I",
        native_aifu_get_and_set, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        CLS_INT_FIELD_UPDATER_IMPL,
        "getAndAdd",
        "(Ljava/lang/Object;I)I",
        native_aifu_get_and_add, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        CLS_INT_FIELD_UPDATER_IMPL,
        "incrementAndGet",
        "(Ljava/lang/Object;)I",
        native_aifu_increment_and_get, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        CLS_INT_FIELD_UPDATER_IMPL,
        "decrementAndGet",
        "(Ljava/lang/Object;)I",
        native_aifu_decrement_and_get, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    // getAndIncrement / getAndDecrement / addAndGet on both impl + base.
    {
        let cls = CLS_INT_FIELD_UPDATER_IMPL;
        r.register(
            cls,
            "getAndIncrement",
            "(Ljava/lang/Object;)I",
            native_aifu_get_and_increment, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            cls,
            "getAndDecrement",
            "(Ljava/lang/Object;)I",
            native_aifu_get_and_decrement, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            cls,
            "addAndGet",
            "(Ljava/lang/Object;I)I",
            native_aifu_add_and_get, cratonvm_native_api::NativeKind::SyntheticStub,
        );
    }
    // NOT on the abstract base. This block used to carry
    // "Also on the abstract base for virtual dispatch." -- and dispatch does not
    // need it, while a user subclass is actively harmed by it.
    //
    // `AtomicIntegerFieldUpdater` is a PUBLIC ABSTRACT class with a protected
    // constructor: an application may extend it and supply its own
    // `get`/`set`/`compareAndSet`. A native registered on the BASE runs in front
    // of that subclass's inherited bodies, so the JDK's own base-class
    // `getAndIncrement` -- specified in terms of `get` and `compareAndSet`, and
    // therefore required to dispatch back INTO the subclass -- never did.
    //
    // MEASURED in compatible mode (`probes/AtomicUpdaterSweep.java`), a counting
    // subclass whose own bodies record every entry:
    //
    //   [subclass] getAndIncrement result             HotSpot 100   CratonVM 10
    //   [subclass] getAndIncrement entered subclass   HotSpot true  CratonVM FALSE
    //   [subclass] getAndIncrement counters           gets=1 cas=1  gets=0 cas=0
    //   [subclass] holder untouched                   HotSpot 10    CratonVM 56
    //
    // The last row is the damaging one: the native read and wrote the caller's
    // HOLDER object through its own slot layout, while the subclass's state --
    // the only state the subclass believes it has -- sat untouched.
    //
    // Why removing them is safe rather than merely better: dispatch probes the
    // registry from the RECEIVER'S OWN class first, and every updater this
    // module hands out is one of the three `$RustJvmImpl` classes, which carry
    // their own full registrations. So these base rows were already DEAD for our
    // own objects and fired only for receivers we should never have intercepted.
    // Since `$RustJvmImpl` now has its real abstract base as superclass, our
    // impls also inherit the base's real bytecode for everything not registered
    // -- which is what made `updateAndGet` and its three siblings work at all.
    //
    // `--real-jdk` was ALREADY clean here (87/87) because strict mode drops
    // these bridges and runs the JDK's own bytecode. This change makes the
    // default mode agree with strict, rather than the other way round.
    //
    // `newUpdater` STAYS on the base: it is a static factory, has no receiver,
    // and is the entry point the whole module exists to serve.
    // Also register accessors on the abstract base AtomicIntegerFieldUpdater
    // so generic-typed callers dispatch correctly without the impl class in scope.
}

fn register_alfu(r: &mut NativeMethodRegistry) {
    r.register(
        CLS_LONG_FIELD_UPDATER,
        "newUpdater",
        "(Ljava/lang/Class;Ljava/lang/String;)Ljava/util/concurrent/atomic/AtomicLongFieldUpdater;",
        native_alfu_new_updater, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        CLS_LONG_FIELD_UPDATER_IMPL,
        "get",
        "(Ljava/lang/Object;)J",
        native_alfu_get, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        CLS_LONG_FIELD_UPDATER_IMPL,
        "set",
        "(Ljava/lang/Object;J)V",
        native_alfu_set, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        CLS_LONG_FIELD_UPDATER_IMPL,
        "lazySet",
        "(Ljava/lang/Object;J)V",
        native_alfu_set, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        CLS_LONG_FIELD_UPDATER_IMPL,
        "compareAndSet",
        "(Ljava/lang/Object;JJ)Z",
        native_alfu_compare_and_set, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        CLS_LONG_FIELD_UPDATER_IMPL,
        "weakCompareAndSet",
        "(Ljava/lang/Object;JJ)Z",
        native_alfu_compare_and_set, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        CLS_LONG_FIELD_UPDATER_IMPL,
        "getAndSet",
        "(Ljava/lang/Object;J)J",
        native_alfu_get_and_set, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        CLS_LONG_FIELD_UPDATER_IMPL,
        "getAndAdd",
        "(Ljava/lang/Object;J)J",
        native_alfu_get_and_add, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    // getAndIncrement / getAndDecrement / addAndGet / incrementAndGet /
    // decrementAndGet on both impl + base. (incrementAndGet/decrementAndGet
    // were previously missing — kotlinx.coroutines' CoroutineScheduler calls
    // incrementAndGet(Object)J, which ABENDed the VM with NoSuchMethodError.)
    {
        let cls = CLS_LONG_FIELD_UPDATER_IMPL;
        r.register(
            cls,
            "getAndIncrement",
            "(Ljava/lang/Object;)J",
            native_alfu_get_and_increment, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            cls,
            "getAndDecrement",
            "(Ljava/lang/Object;)J",
            native_alfu_get_and_decrement, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            cls,
            "addAndGet",
            "(Ljava/lang/Object;J)J",
            native_alfu_add_and_get, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            cls,
            "incrementAndGet",
            "(Ljava/lang/Object;)J",
            native_alfu_increment_and_get, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            cls,
            "decrementAndGet",
            "(Ljava/lang/Object;)J",
            native_alfu_decrement_and_get, cratonvm_native_api::NativeKind::SyntheticStub,
        );
    }
    // Also register accessors on the abstract base class so virtual
    // dispatch from generic code that holds an AtomicLongFieldUpdater
    // reference (e.g. ActiveMQ's LongSequenceGenerator.setLastSequenceId)
    // still finds an implementation. Mirrors the ARFU layout above —
    // without these, every AtomicLongFieldUpdater.set/get call from a
    // user library throws AbstractMethodError before the Impl class is
    // ever consulted.
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests;

#[cfg(test)]
mod r21_updaters_value_check_tests;
