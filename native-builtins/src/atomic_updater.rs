// SPDX-License-Identifier: Apache-2.0
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
//!   `IllegalArgumentException` before any heap activity.
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
//! ## `--jdk-only`: this whole module is NOT registered (2026-08-12)
//!
//! Everything above describes **Compatible** mode and is unchanged there.
//! Under [`CompatibilityMode::JdkOnly`] `register_atomic_updater_natives`
//! registers nothing at all — see the guard at the top of it.
//!
//! The reason is measured, not argued.
//! `docs/known-issues/jdk-only/APP-READINESS-20260812.md` §3.3 records that
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
//! The premise is about a **synthetic** `Field`. Under `--jdk-only` there is no
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
pub(crate) const CLS_REF_FIELD_UPDATER_IMPL: &str =
    "java/util/concurrent/atomic/AtomicReferenceFieldUpdater$RustJvmImpl";
pub(crate) const CLS_INT_FIELD_UPDATER_IMPL: &str =
    "java/util/concurrent/atomic/AtomicIntegerFieldUpdater$RustJvmImpl";
pub(crate) const CLS_LONG_FIELD_UPDATER_IMPL: &str =
    "java/util/concurrent/atomic/AtomicLongFieldUpdater$RustJvmImpl";

// ---------------------------------------------------------------------------
// Synthetic-impl slot layout
// ---------------------------------------------------------------------------

pub(crate) const FU_SLOT_TCLASS_ID: usize = 0;
pub(crate) const FU_SLOT_FIELD_INDEX: usize = 1;
pub(crate) const FU_SLOT_DESC_TAG: usize = 2;
pub(crate) const FU_SLOT_VCLASS_ID: usize = 3;
pub(crate) const FU_NUM_SLOTS: usize = 4;

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
        // Fallible since 2026-08-10 (JDK-only wave 2, step 3): the
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
    // HotSpot's wording for THIS variant's type refusal, transcribed:
    // "Must be integer type" / "Must be long type". The reference variant
    // never reaches it -- it leaves by `ClassCastException` -- and passes "".
    type_message: &str,
) -> MethodCallResult {
    validate_field_name(field_name).map_err(MethodCallFailed::from)?;

    let tclass_id = mirror_class_id(ctx, tclass_mirror).ok_or_else(|| {
        MethodCallFailed::from(RuntimeError::IllegalArgumentException {
            message: "tclass is not a regular class mirror".to_string(),
        })
    })?;

    // Find the named field on tclass (and any superclass).
    let mut found: Option<cratonvm_native_api::FieldMetadata> = None;
    let mut cursor = Some(tclass_id);
    while let Some(cid) = cursor {
        let fields = ctx.declared_fields(cid);
        if let Some(meta) = fields.into_iter().find(|m| m.name == field_name) {
            found = Some(meta);
            break;
        }
        cursor = ctx.superclass_of(cid);
    }
    let meta = found.ok_or_else(|| {
        tracing::warn!(
            tclass = ?ctx.class_name_of_id(tclass_id),
            field = %field_name,
            "T19.H5: newUpdater field not found"
        );
        new_updater_refusal(
            ctx,
            &format!("java.lang.NoSuchFieldException: {field_name}"),
        )
    })?;

    // THE ORDER IS THE ORACLE'S, and it is measured rather than assumed
    // (`apps/probes/AtomicFamilySweep.java`): a field that fails two checks at
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
        return Err(RuntimeError::ClassCastException {
            // No message: HotSpot's is null here (measured).
            message: String::new(),
        }
        .into());
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
    let Some(vclass) = arg_obj(args, 1) else {
        return Err(new_updater_refusal(
            ctx,
            "java.lang.NullPointerException: vclass",
        ));
    };
    let Some(name_obj) = arg_obj(args, 2) else {
        return Err(new_updater_refusal(
            ctx,
            "java.lang.NullPointerException: fieldName",
        ));
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
        Some(vclass),
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
        return Err(new_updater_refusal(
            ctx,
            "java.lang.NullPointerException: fieldName",
        ));
    };
    let name = ctx.read_string(name_obj).unwrap_or_default();
    build_updater(
        ctx,
        tclass,
        &name,
        descriptor_tag_for_int_variant,
        CLS_INT_FIELD_UPDATER_IMPL,
        None,
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
        return Err(new_updater_refusal(
            ctx,
            "java.lang.NullPointerException: fieldName",
        ));
    };
    let name = ctx.read_string(name_obj).unwrap_or_default();
    build_updater(
        ctx,
        tclass,
        &name,
        descriptor_tag_for_long_variant,
        CLS_LONG_FIELD_UPDATER_IMPL,
        None,
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
    ctx: &dyn NativeContext,
    updater: ObjectRef,
    args: &[Value],
    idx: usize,
) -> Result<ObjectRef, MethodCallFailed> {
    let Some(target) = (match args.get(idx) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    }) else {
        // Null target: `isInstance(null)` is false, so this is a
        // ClassCastException, not the NullPointerException `arg_obj_or_npe`
        // would raise.
        return Err(RuntimeError::ClassCastException {
            message: "the target object is not an instance of the updater's class".to_string(),
        }
        .into());
    };
    if let Value::Int(tclass_raw) = ctx.get_field(updater, FU_SLOT_TCLASS_ID) {
        if tclass_raw >= 0 {
            let tclass = cratonvm_types::ClassId::new(tclass_raw as u32);
            let actual = ctx.class_id_of_object(target);
            if actual != tclass && !ctx.is_subclass(actual, tclass) {
                return Err(RuntimeError::ClassCastException {
                    message: format!(
                        "{} is not an instance of {}",
                        ctx.class_name_of_id(actual).unwrap_or_default(),
                        ctx.class_name_of_id(tclass).unwrap_or_default()
                    ),
                }
                .into());
            }
        }
    }
    Ok(target)
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
    let slot = impl_slot(ctx, this).unwrap_or(0);
    let ok = ctx.compare_and_swap_field(target, slot, expected, new_val);
    Ok(Some(Value::Int(if ok { 1 } else { 0 })))
}

fn native_arfu_get_and_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = arg_obj_or_npe(args, 0, "updater")?;
    let target = require_target(ctx, this, args, 1)?;
    let new_val = arg_value(args, 2);
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
    // The UnaryOperator is arbitrary Java code and cannot run inside the
    // CAS lock, so we recompute-and-CAS-retry until the swap commits —
    // exactly HotSpot's getAndUpdate. The loop must be unbounded: bailing
    // after a fixed retry count (the previous behaviour) abandoned the
    // update under contention, leaving the field unchanged but pretending
    // success — a lost update.
    arfu_update_with_operator(ctx, target, slot, op, false)
}

fn native_arfu_update_and_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = arg_obj_or_npe(args, 0, "updater")?;
    let target = require_target(ctx, this, args, 1)?;
    let op = arg_obj_or_npe(args, 2, "op")?;
    let slot = impl_slot(ctx, this).unwrap_or(0);
    // Unbounded recompute-and-CAS-retry (see native_arfu_get_and_update):
    // bailing after a fixed retry count would abandon the update and
    // return a value never actually stored.
    arfu_update_with_operator(ctx, target, slot, op, true)
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
    Ok(Some(Value::Long(v)))
}

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
/// push `native-builtins/tests/stub_ratchet.rs::synthetic_stub_count_does_not_regress`
/// (baseline 1263, `SLACK = 0`) over its frozen ceiling and move ~31 rows in
/// `scripts/baselines/jdk-only-kind-map-25-linux.tsv`. That ratchet's own
/// failure text says not to raise the baseline. Compatible mode must come out
/// of this change bit-identical, and with the `return` it does: same rows, same
/// kinds, same counts.
pub fn register_atomic_updater_natives(registry: &mut NativeMethodRegistry) {
    if registry.compatibility_mode() == cratonvm_types::compat::CompatibilityMode::JdkOnly {
        tracing::info!(
            "T19.H5: --jdk-only — NOT registering the Atomic{{Reference,Integer,Long}}FieldUpdater \
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
        native_arfu_new_updater,
    );
    // Accessor methods on the synthetic impl.
    r.register(
        CLS_REF_FIELD_UPDATER_IMPL,
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_arfu_get,
    );
    r.register(
        CLS_REF_FIELD_UPDATER_IMPL,
        "set",
        "(Ljava/lang/Object;Ljava/lang/Object;)V",
        native_arfu_set,
    );
    r.register(
        CLS_REF_FIELD_UPDATER_IMPL,
        "lazySet",
        "(Ljava/lang/Object;Ljava/lang/Object;)V",
        native_arfu_lazy_set,
    );
    r.register(
        CLS_REF_FIELD_UPDATER_IMPL,
        "compareAndSet",
        "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Z",
        native_arfu_compare_and_set,
    );
    r.register(
        CLS_REF_FIELD_UPDATER_IMPL,
        "weakCompareAndSet",
        "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Z",
        native_arfu_compare_and_set,
    );
    r.register(
        CLS_REF_FIELD_UPDATER_IMPL,
        "getAndSet",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_arfu_get_and_set,
    );
    r.register(
        CLS_REF_FIELD_UPDATER_IMPL,
        "getAndUpdate",
        "(Ljava/lang/Object;Ljava/util/function/UnaryOperator;)Ljava/lang/Object;",
        native_arfu_get_and_update,
    );
    r.register(
        CLS_REF_FIELD_UPDATER_IMPL,
        "updateAndGet",
        "(Ljava/lang/Object;Ljava/util/function/UnaryOperator;)Ljava/lang/Object;",
        native_arfu_update_and_get,
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
        native_aifu_new_updater,
    );
    r.register(
        CLS_INT_FIELD_UPDATER_IMPL,
        "get",
        "(Ljava/lang/Object;)I",
        native_aifu_get,
    );
    r.register(
        CLS_INT_FIELD_UPDATER_IMPL,
        "set",
        "(Ljava/lang/Object;I)V",
        native_aifu_set,
    );
    r.register(
        CLS_INT_FIELD_UPDATER_IMPL,
        "lazySet",
        "(Ljava/lang/Object;I)V",
        native_aifu_set,
    );
    r.register(
        CLS_INT_FIELD_UPDATER_IMPL,
        "compareAndSet",
        "(Ljava/lang/Object;II)Z",
        native_aifu_compare_and_set,
    );
    r.register(
        CLS_INT_FIELD_UPDATER_IMPL,
        "weakCompareAndSet",
        "(Ljava/lang/Object;II)Z",
        native_aifu_compare_and_set,
    );
    r.register(
        CLS_INT_FIELD_UPDATER_IMPL,
        "getAndSet",
        "(Ljava/lang/Object;I)I",
        native_aifu_get_and_set,
    );
    r.register(
        CLS_INT_FIELD_UPDATER_IMPL,
        "getAndAdd",
        "(Ljava/lang/Object;I)I",
        native_aifu_get_and_add,
    );
    r.register(
        CLS_INT_FIELD_UPDATER_IMPL,
        "incrementAndGet",
        "(Ljava/lang/Object;)I",
        native_aifu_increment_and_get,
    );
    r.register(
        CLS_INT_FIELD_UPDATER_IMPL,
        "decrementAndGet",
        "(Ljava/lang/Object;)I",
        native_aifu_decrement_and_get,
    );
    // getAndIncrement / getAndDecrement / addAndGet on both impl + base.
    for cls in [CLS_INT_FIELD_UPDATER_IMPL] {
        r.register(
            cls,
            "getAndIncrement",
            "(Ljava/lang/Object;)I",
            native_aifu_get_and_increment,
        );
        r.register(
            cls,
            "getAndDecrement",
            "(Ljava/lang/Object;)I",
            native_aifu_get_and_decrement,
        );
        r.register(
            cls,
            "addAndGet",
            "(Ljava/lang/Object;I)I",
            native_aifu_add_and_get,
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
    // `--jdk-only` was ALREADY clean here (87/87) because strict mode drops
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
        native_alfu_new_updater,
    );
    r.register(
        CLS_LONG_FIELD_UPDATER_IMPL,
        "get",
        "(Ljava/lang/Object;)J",
        native_alfu_get,
    );
    r.register(
        CLS_LONG_FIELD_UPDATER_IMPL,
        "set",
        "(Ljava/lang/Object;J)V",
        native_alfu_set,
    );
    r.register(
        CLS_LONG_FIELD_UPDATER_IMPL,
        "lazySet",
        "(Ljava/lang/Object;J)V",
        native_alfu_set,
    );
    r.register(
        CLS_LONG_FIELD_UPDATER_IMPL,
        "compareAndSet",
        "(Ljava/lang/Object;JJ)Z",
        native_alfu_compare_and_set,
    );
    r.register(
        CLS_LONG_FIELD_UPDATER_IMPL,
        "weakCompareAndSet",
        "(Ljava/lang/Object;JJ)Z",
        native_alfu_compare_and_set,
    );
    r.register(
        CLS_LONG_FIELD_UPDATER_IMPL,
        "getAndSet",
        "(Ljava/lang/Object;J)J",
        native_alfu_get_and_set,
    );
    r.register(
        CLS_LONG_FIELD_UPDATER_IMPL,
        "getAndAdd",
        "(Ljava/lang/Object;J)J",
        native_alfu_get_and_add,
    );
    // getAndIncrement / getAndDecrement / addAndGet / incrementAndGet /
    // decrementAndGet on both impl + base. (incrementAndGet/decrementAndGet
    // were previously missing — kotlinx.coroutines' CoroutineScheduler calls
    // incrementAndGet(Object)J, which ABENDed the VM with NoSuchMethodError.)
    for cls in [CLS_LONG_FIELD_UPDATER_IMPL] {
        r.register(
            cls,
            "getAndIncrement",
            "(Ljava/lang/Object;)J",
            native_alfu_get_and_increment,
        );
        r.register(
            cls,
            "getAndDecrement",
            "(Ljava/lang/Object;)J",
            native_alfu_get_and_decrement,
        );
        r.register(
            cls,
            "addAndGet",
            "(Ljava/lang/Object;J)J",
            native_alfu_add_and_get,
        );
        r.register(
            cls,
            "incrementAndGet",
            "(Ljava/lang/Object;)J",
            native_alfu_increment_and_get,
        );
        r.register(
            cls,
            "decrementAndGet",
            "(Ljava/lang/Object;)J",
            native_alfu_decrement_and_get,
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
mod tests {
    use super::*;
    use crate::test_utils::{mock_ctx, MockNativeContext};
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Build a Class mirror in the mock and return both the mirror ref
    /// and the underlying ClassId for assertion convenience.
    fn make_class_mirror(ctx: &mut MockNativeContext, name: &str) -> (ObjectRef, ClassId) {
        let cid = ctx.ensure_class_initialized(name).unwrap();
        let mirror = ctx.get_class_mirror(cid);
        (mirror, cid)
    }

    /// The mock's `declared_fields` returns empty; tests that need
    /// FieldMetadata install a custom override via the mock's hooks.
    /// Since the mock doesn't currently expose declared_fields override,
    /// we wrap by sub-classing in a dedicated sub-mock for these tests.
    /// To keep the diff minimal, the tests use a bypass mock.
    use cratonvm_native_api::FieldMetadata;
    use std::cell::UnsafeCell;
    use std::collections::HashMap;

    /// Test wrapper that supplements the standard MockNativeContext with
    /// per-class FieldMetadata so the updater natives can find a real
    /// field at a deterministic slot.
    struct UpdaterMock {
        inner: MockNativeContext,
        field_table: UnsafeCell<HashMap<u32, Vec<(String, String, u16, usize, bool)>>>,
    }

    impl UpdaterMock {
        fn new() -> Self {
            Self {
                inner: mock_ctx(),
                field_table: UnsafeCell::new(HashMap::new()),
            }
        }

        fn add_field(
            &self,
            cid: ClassId,
            name: &str,
            descriptor: &str,
            access_flags: u16,
            slot_index: usize,
            is_static: bool,
        ) {
            // SAFETY: single-threaded test code.
            let table = unsafe { &mut *self.field_table.get() };
            table.entry(cid.as_u32()).or_default().push((
                name.to_string(),
                descriptor.to_string(),
                access_flags,
                slot_index,
                is_static,
            ));
        }
    }

    // Forward every NativeContext method to `inner` except declared_fields.
    impl cratonvm_native_api::NativeClassAccess for UpdaterMock {
        fn load_class(&mut self, name: &str) -> MethodCallResult {
            self.inner.load_class(name)
        }
        fn class_name_of_id(&self, class_id: ClassId) -> Option<String> {
            self.inner.class_name_of_id(class_id)
        }
        fn class_id_of_object(&self, obj: ObjectRef) -> ClassId {
            self.inner.class_id_of_object(obj)
        }
        fn method_exists(&self, c: &str, m: &str, d: &str) -> bool {
            self.inner.method_exists(c, m, d)
        }
        fn ensure_class_initialized(&mut self, name: &str) -> Result<ClassId, MethodCallFailed> {
            self.inner.ensure_class_initialized(name)
        }
        fn is_subclass(&self, c: ClassId, p: ClassId) -> bool {
            self.inner.is_subclass(c, p)
        }
        fn superclass_of(&self, class_id: ClassId) -> Option<ClassId> {
            self.inner.superclass_of(class_id)
        }
        fn class_id_by_name(&self, name: &str) -> Option<ClassId> {
            self.inner.class_id_by_name(name)
        }
        fn loader_id_of_class(&self, class_id: ClassId) -> i32 {
            self.inner.loader_id_of_class(class_id)
        }
        fn is_record_class(&self, class_id: ClassId) -> bool {
            self.inner.is_record_class(class_id)
        }
        fn record_components(&self, class_id: ClassId) -> Vec<(String, String)> {
            self.inner.record_components(class_id)
        }
        fn is_sealed_class(&self, class_id: ClassId) -> bool {
            self.inner.is_sealed_class(class_id)
        }
        fn permitted_subclasses(&self, class_id: ClassId) -> Vec<String> {
            self.inner.permitted_subclasses(class_id)
        }
        fn declared_fields(&self, class_id: ClassId) -> Vec<FieldMetadata> {
            // SAFETY: single-threaded test code.
            let table = unsafe { &*self.field_table.get() };
            match table.get(&class_id.as_u32()) {
                Some(entries) => entries
                    .iter()
                    .map(|(name, desc, flags, slot, is_static)| FieldMetadata {
                        name: name.clone(),
                        descriptor: desc.clone(),
                        access_flags: *flags,
                        slot_index: *slot,
                        declaring_class_id: class_id,
                        is_static: *is_static,
                    })
                    .collect(),
                None => Vec::new(),
            }
        }
        fn declared_methods(&self, class_id: ClassId) -> Vec<cratonvm_native_api::MethodMetadata> {
            self.inner.declared_methods(class_id)
        }
        fn class_interfaces(&self, class_id: ClassId) -> Vec<ClassId> {
            self.inner.class_interfaces(class_id)
        }
        fn class_access_flags(&self, class_id: ClassId) -> u16 {
            self.inner.class_access_flags(class_id)
        }
        fn primitive_class_mirror(&mut self, name: &str) -> ObjectRef {
            self.inner.primitive_class_mirror(name)
        }
        fn class_annotations(&self, class_id: ClassId) -> Vec<cratonvm_native_api::AnnotationData> {
            self.inner.class_annotations(class_id)
        }
        fn method_annotations(
            &self,
            class_id: ClassId,
            m: &str,
            d: &str,
        ) -> Vec<cratonvm_native_api::AnnotationData> {
            self.inner.method_annotations(class_id, m, d)
        }
        fn field_annotations(
            &self,
            class_id: ClassId,
            f: &str,
        ) -> Vec<cratonvm_native_api::AnnotationData> {
            self.inner.field_annotations(class_id, f)
        }
        fn method_parameter_annotations(
            &self,
            class_id: ClassId,
            m: &str,
            d: &str,
        ) -> Vec<Vec<cratonvm_native_api::AnnotationData>> {
            self.inner.method_parameter_annotations(class_id, m, d)
        }
        fn class_signature(&self, class_id: ClassId) -> Option<String> {
            self.inner.class_signature(class_id)
        }
        fn method_signature(&self, class_id: ClassId, m: &str, d: &str) -> Option<String> {
            self.inner.method_signature(class_id, m, d)
        }
        fn field_signature(&self, class_id: ClassId, f: &str) -> Option<String> {
            self.inner.field_signature(class_id, f)
        }
        fn method_annotation_default(
            &self,
            class_id: ClassId,
            m: &str,
            d: &str,
        ) -> Option<cratonvm_native_api::AnnotationElementValue> {
            self.inner.method_annotation_default(class_id, m, d)
        }
        fn module_name_of_class(&self, class_id: ClassId) -> Option<String> {
            self.inner.module_name_of_class(class_id)
        }
        fn find_resource(&self, name: &str) -> Option<Vec<u8>> {
            self.inner.find_resource(name)
        }
        fn list_application_class_names(&self) -> Vec<String> {
            self.inner.list_application_class_names()
        }
        fn register_dynamic_classpath(&mut self, paths: &[String]) {
            self.inner.register_dynamic_classpath(paths)
        }
        fn define_class_from_bytes(&mut self, name: &str, bytes: &[u8]) -> Option<ClassId> {
            self.inner.define_class_from_bytes(name, bytes)
        }
        fn define_class_with_loader(
            &mut self,
            name: &str,
            bytes: &[u8],
            loader_id: u32,
        ) -> Option<ClassId> {
            self.inner.define_class_with_loader(name, bytes, loader_id)
        }
        fn class_id_by_name_and_loader(&self, name: &str, loader_id: u32) -> Option<ClassId> {
            self.inner.class_id_by_name_and_loader(name, loader_id)
        }
        fn allocate_loader_id(&mut self) -> u32 {
            self.inner.allocate_loader_id()
        }
        fn is_package_exported_unqualified(&self, module_name: &str, pkg: &str) -> bool {
            self.inner.is_package_exported_unqualified(module_name, pkg)
        }
        fn is_package_exported_to(&self, module_name: &str, pkg: &str, to_module: &str) -> bool {
            self.inner
                .is_package_exported_to(module_name, pkg, to_module)
        }
        fn is_package_open_unqualified(&self, module_name: &str, pkg: &str) -> bool {
            self.inner.is_package_open_unqualified(module_name, pkg)
        }
        fn is_package_open_to(&self, module_name: &str, pkg: &str, to_module: &str) -> bool {
            self.inner.is_package_open_to(module_name, pkg, to_module)
        }
        fn check_deep_reflection_access(
            &self,
            accessor_class_id: ClassId,
            target_class_id: ClassId,
        ) -> Result<(), String> {
            self.inner
                .check_deep_reflection_access(accessor_class_id, target_class_id)
        }
    }

    impl cratonvm_native_api::NativeInvokeAccess for UpdaterMock {
        fn invoke(
            &mut self,
            class_name: &str,
            method_name: &str,
            descriptor: &str,
            args: &[Value],
        ) -> MethodCallResult {
            self.inner.invoke(class_name, method_name, descriptor, args)
        }
        fn invoke_virtual(
            &mut self,
            receiver: ObjectRef,
            method_name: &str,
            descriptor: &str,
            args: &[Value],
        ) -> MethodCallResult {
            self.inner
                .invoke_virtual(receiver, method_name, descriptor, args)
        }
    }

    impl cratonvm_native_api::NativeHeapAccess for UpdaterMock {
        fn new_object(&mut self, class_name: &str) -> MethodCallResult {
            self.inner.new_object(class_name)
        }
        fn identity_hash_code(&self, obj: ObjectRef) -> i32 {
            self.inner.identity_hash_code(obj)
        }
        fn get_field(&self, obj: ObjectRef, index: usize) -> Value {
            self.inner.get_field(obj, index)
        }
        fn set_field(&self, obj: ObjectRef, index: usize, value: Value) {
            self.inner.set_field(obj, index, value)
        }
        fn get_field_by_name(&self, obj: ObjectRef, field_name: &str) -> Value {
            self.inner.get_field_by_name(obj, field_name)
        }
        fn set_field_by_name(&self, obj: ObjectRef, field_name: &str, value: Value) {
            self.inner.set_field_by_name(obj, field_name, value)
        }
        fn resolve_field_index(&self, c: &str, f: &str) -> Option<usize> {
            self.inner.resolve_field_index(c, f)
        }
        fn resolve_field_index_by_class_id(&self, c: ClassId, f: &str) -> Option<usize> {
            self.inner.resolve_field_index_by_class_id(c, f)
        }
        fn new_array(
            &mut self,
            element_type: cratonvm_types::ArrayElementType,
            length: usize,
        ) -> ObjectRef {
            self.inner.new_array(element_type, length)
        }
        fn new_ref_array(&mut self, class_id: ClassId, length: usize) -> ObjectRef {
            self.inner.new_ref_array(class_id, length)
        }
        fn array_length(&self, obj: ObjectRef) -> usize {
            self.inner.array_length(obj)
        }
        fn get_array_element(&self, obj: ObjectRef, index: usize) -> Value {
            self.inner.get_array_element(obj, index)
        }
        fn set_array_element(&self, obj: ObjectRef, index: usize, value: Value) {
            self.inner.set_array_element(obj, index, value)
        }
        fn heap_kind_of(&self, obj: ObjectRef) -> cratonvm_types::ObjectKind {
            self.inner.heap_kind_of(obj)
        }
        fn heap_element_type_of(&self, obj: ObjectRef) -> cratonvm_types::ArrayElementType {
            self.inner.heap_element_type_of(obj)
        }
        fn create_string(&mut self, text: &str) -> ObjectRef {
            self.inner.create_string(text)
        }
        fn read_string(&self, obj: ObjectRef) -> Option<String> {
            self.inner.read_string(obj)
        }
        fn get_class_mirror(&mut self, class_id: ClassId) -> ObjectRef {
            self.inner.get_class_mirror(class_id)
        }
        fn alloc_object(&mut self, class_id: ClassId, num_fields: usize) -> ObjectRef {
            self.inner.alloc_object(class_id, num_fields)
        }
        fn object_num_fields(&self, obj: ObjectRef) -> usize {
            self.inner.object_num_fields(obj)
        }
        fn get_field_volatile(&self, obj: ObjectRef, index: usize) -> Value {
            self.inner.get_field_volatile(obj, index)
        }
        fn set_field_volatile(&self, obj: ObjectRef, index: usize, value: Value) {
            self.inner.set_field_volatile(obj, index, value)
        }
        fn compare_and_swap_field(
            &mut self,
            obj: ObjectRef,
            index: usize,
            expected: Value,
            new_val: Value,
        ) -> bool {
            self.inner
                .compare_and_swap_field(obj, index, expected, new_val)
        }
        fn allocate_instance(&mut self, class_name: &str) -> Option<ObjectRef> {
            self.inner.allocate_instance(class_name)
        }
        fn discover_reference(
            &mut self,
            ref_type: u8,
            reference_obj: ObjectRef,
            referent: ObjectRef,
            queue: Option<ObjectRef>,
        ) {
            self.inner
                .discover_reference(ref_type, reference_obj, referent, queue)
        }
        fn heap_allocated_bytes(&self) -> usize {
            self.inner.heap_allocated_bytes()
        }
    }

    impl cratonvm_native_api::NativeThreadAccess for UpdaterMock {
        fn thread_id(&self) -> u64 {
            self.inner.thread_id()
        }
        fn monitor_enter(&mut self, obj: ObjectRef) {
            self.inner.monitor_enter(obj)
        }
        fn monitor_exit(&mut self, obj: ObjectRef) {
            self.inner.monitor_exit(obj)
        }
        fn monitor_wait(&mut self, obj: ObjectRef, timeout_ms: Option<u64>) -> MethodCallResult {
            self.inner.monitor_wait(obj, timeout_ms)
        }
        fn monitor_notify(&mut self, obj: ObjectRef) -> MethodCallResult {
            self.inner.monitor_notify(obj)
        }
        fn monitor_notify_all(&mut self, obj: ObjectRef) -> MethodCallResult {
            self.inner.monitor_notify_all(obj)
        }
        fn thread_start(&mut self, thread_obj: ObjectRef) -> MethodCallResult {
            self.inner.thread_start(thread_obj)
        }
        fn thread_join(&mut self, thread_obj: ObjectRef) -> MethodCallResult {
            self.inner.thread_join(thread_obj)
        }
        fn thread_is_alive(&self, thread_obj: ObjectRef) -> bool {
            self.inner.thread_is_alive(thread_obj)
        }
        fn current_thread_object(&mut self) -> ObjectRef {
            self.inner.current_thread_object()
        }
        fn thread_interrupt(&mut self, thread_obj: ObjectRef) {
            self.inner.thread_interrupt(thread_obj)
        }
        fn is_interrupted(&self, clear: bool) -> bool {
            self.inner.is_interrupted(clear)
        }
        fn park(&mut self, timeout: Option<std::time::Duration>) {
            self.inner.park(timeout)
        }
        fn unpark(&self, thread_obj: ObjectRef) {
            self.inner.unpark(thread_obj)
        }
        fn get_scoped_value(&self, key_id: u64) -> Option<Value> {
            self.inner.get_scoped_value(key_id)
        }
        fn push_scoped_value(&mut self, key_id: u64, value: Value) {
            self.inner.push_scoped_value(key_id, value)
        }
        fn pop_scoped_value(&mut self) {
            self.inner.pop_scoped_value()
        }
        fn scoped_value_depth(&self) -> usize {
            self.inner.scoped_value_depth()
        }
        fn active_thread_count(&self) -> i32 {
            self.inner.active_thread_count()
        }
        fn enumerate_threads(&self, max: usize) -> Vec<ObjectRef> {
            self.inner.enumerate_threads(max)
        }
    }

    impl cratonvm_native_api::NativeExceptionAccess for UpdaterMock {
        fn capture_stack_trace(
            &mut self,
            throwable_hash: i32,
        ) -> Vec<cratonvm_native_api::StackTraceEntry> {
            self.inner.capture_stack_trace(throwable_hash)
        }
        fn get_stack_trace(
            &self,
            throwable_hash: i32,
        ) -> Option<Vec<cratonvm_native_api::StackTraceEntry>> {
            self.inner.get_stack_trace(throwable_hash)
        }
    }

    impl cratonvm_native_api::NativeGpuAccess for UpdaterMock {}

    impl cratonvm_native_api::NativeSystemAccess for UpdaterMock {
        fn record_printed_value(&mut self, value: Value) {
            self.inner.record_printed_value(value)
        }
        fn record_printed_line(&mut self, text: String) {
            self.inner.record_printed_line(text)
        }
        fn get_system_stream(&self, name: &str) -> Option<ObjectRef> {
            self.inner.get_system_stream(name)
        }
        fn get_system_property(&self, key: &str) -> Option<String> {
            self.inner.get_system_property(key)
        }
        fn set_system_property(&mut self, key: &str, value: &str) -> Option<String> {
            self.inner.set_system_property(key, value)
        }
        fn is_interface_class(&self, class_id: ClassId) -> bool {
            self.inner.is_interface_class(class_id)
        }
        fn get_static_field(&self, class_id: ClassId, field_index: usize) -> Value {
            self.inner.get_static_field(class_id, field_index)
        }
        fn set_static_field(&mut self, class_id: ClassId, field_index: usize, value: Value) {
            self.inner.set_static_field(class_id, field_index, value)
        }
        fn fd_table(&self) -> &cratonvm_native_api::fd_table::FileDescriptorTable {
            self.inner.fd_table()
        }
        fn allocate_native_memory(&mut self, size: usize, align: usize) -> Option<(i64, *mut u8)> {
            self.inner.allocate_native_memory(size, align)
        }
        fn free_native_memory(&mut self, alloc_id: i64) {
            self.inner.free_native_memory(alloc_id)
        }
        fn load_native_library(&mut self, path: &str) -> Result<i64, MethodCallFailed> {
            self.inner.load_native_library(path)
        }
        fn find_native_symbol(&self, lib_index: i64, name: &str) -> Option<usize> {
            self.inner.find_native_symbol(lib_index, name)
        }
        fn register_upcall(&mut self, entry: cratonvm_native_api::ffi::UpcallEntry) -> usize {
            self.inner.register_upcall(entry)
        }
        fn get_upcall_info(&self, slot: usize) -> Option<(ObjectRef, Vec<i32>, i32)> {
            self.inner.get_upcall_info(slot)
        }
        fn loaded_class_count(&self) -> usize {
            self.inner.loaded_class_count()
        }
        fn gc_collection_count(&self) -> u64 {
            self.inner.gc_collection_count()
        }
        fn force_gc(&mut self) {
            self.inner.force_gc()
        }
    }

    fn make_class_mirror_um(um: &mut UpdaterMock, name: &str) -> (ObjectRef, ClassId) {
        make_class_mirror(&mut um.inner, name)
    }

    static ARFU_TARGET_OLD: AtomicUsize = AtomicUsize::new(0);
    static ARFU_TARGET_NEW: AtomicUsize = AtomicUsize::new(0);
    static ARFU_OP_OLD: AtomicUsize = AtomicUsize::new(0);
    static ARFU_OP_NEW: AtomicUsize = AtomicUsize::new(0);
    static ARFU_PREV_OLD: AtomicUsize = AtomicUsize::new(0);
    static ARFU_PREV_NEW: AtomicUsize = AtomicUsize::new(0);
    static ARFU_APPLIED: AtomicUsize = AtomicUsize::new(0);
    static ARFU_APPLY_CALLS: AtomicUsize = AtomicUsize::new(0);

    fn relocating_arfu_apply(
        ctx: &mut MockNativeContext,
        receiver: ObjectRef,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> Option<MethodCallResult> {
        if (method_name, descriptor) != ("apply", "(Ljava/lang/Object;)Ljava/lang/Object;") {
            return None;
        }
        ARFU_APPLY_CALLS.fetch_add(1, Ordering::SeqCst);
        assert_eq!(
            receiver.as_ptr() as usize,
            ARFU_OP_OLD.load(Ordering::SeqCst)
        );
        assert_eq!(
            args.first().copied(),
            Some(Value::Object(Some(unsafe {
                ObjectRef::from_raw(ARFU_PREV_OLD.load(Ordering::SeqCst) as *mut u8)
            })))
        );
        ctx.remap_native_pin_addr_for_test(
            ARFU_TARGET_OLD.load(Ordering::SeqCst),
            ARFU_TARGET_NEW.load(Ordering::SeqCst),
        );
        ctx.remap_native_pin_addr_for_test(
            ARFU_OP_OLD.load(Ordering::SeqCst),
            ARFU_OP_NEW.load(Ordering::SeqCst),
        );
        ctx.remap_native_pin_addr_for_test(
            ARFU_PREV_OLD.load(Ordering::SeqCst),
            ARFU_PREV_NEW.load(Ordering::SeqCst),
        );
        Some(Ok(Some(Value::Object(Some(unsafe {
            ObjectRef::from_raw(ARFU_APPLIED.load(Ordering::SeqCst) as *mut u8)
        })))))
    }

    // -----------------------------------------------------------------
    // T19.H5: validation-only tests
    // -----------------------------------------------------------------

    #[test]
    fn t19_h5_validate_field_name_accepts_normal_identifiers() {
        assert!(validate_field_name("next").is_ok());
        assert!(validate_field_name("FIELD_42").is_ok());
        assert!(validate_field_name("$inner$0").is_ok());
        assert!(validate_field_name("a").is_ok());
    }

    #[test]
    fn t19_h5_validate_field_name_rejects_dangerous_inputs() {
        assert!(validate_field_name("").is_err());
        assert!(validate_field_name(&"a".repeat(257)).is_err());
        assert!(validate_field_name("../traversal").is_err());
        assert!(validate_field_name("foo/bar").is_err());
        assert!(validate_field_name("foo\\bar").is_err());
        assert!(validate_field_name("foo:bar").is_err());
        assert!(validate_field_name("foo\nbar").is_err());
        assert!(validate_field_name("foo\0bar").is_err());
        assert!(validate_field_name("foo\x7Fbar").is_err());
    }

    #[test]
    fn t19_h5_descriptor_tag_helpers_match_jdk_acceptance() {
        assert_eq!(descriptor_tag_for_int_variant("I"), Some(DESC_TAG_INT));
        assert_eq!(descriptor_tag_for_int_variant("Z"), Some(DESC_TAG_INT));
        assert_eq!(descriptor_tag_for_int_variant("J"), None); // long rejected
        assert_eq!(descriptor_tag_for_int_variant("Ljava/lang/Object;"), None);
        assert_eq!(descriptor_tag_for_long_variant("J"), Some(DESC_TAG_LONG));
        assert_eq!(descriptor_tag_for_long_variant("I"), None);
        assert!(descriptor_is_reference("Ljava/lang/Object;"));
        assert!(descriptor_is_reference("[I"));
        assert!(!descriptor_is_reference("I"));
    }

    #[test]
    fn t19_h5_arfu_update_rereads_pins_after_operator_gc() {
        let mut ctx = mock_ctx();
        let target_old = ctx.fresh_object_ref();
        let target_new = ctx.fresh_object_ref();
        let op_old = ctx.fresh_object_ref();
        let op_new = ctx.fresh_object_ref();
        let prev_old = ctx.fresh_object_ref();
        let prev_new = ctx.fresh_object_ref();
        let applied = ctx.fresh_object_ref();
        let slot = 1;

        ctx.set_field(target_old, slot, Value::Object(Some(prev_old)));
        ctx.set_field(target_new, slot, Value::Object(Some(prev_new)));

        ARFU_TARGET_OLD.store(target_old.as_ptr() as usize, Ordering::SeqCst);
        ARFU_TARGET_NEW.store(target_new.as_ptr() as usize, Ordering::SeqCst);
        ARFU_OP_OLD.store(op_old.as_ptr() as usize, Ordering::SeqCst);
        ARFU_OP_NEW.store(op_new.as_ptr() as usize, Ordering::SeqCst);
        ARFU_PREV_OLD.store(prev_old.as_ptr() as usize, Ordering::SeqCst);
        ARFU_PREV_NEW.store(prev_new.as_ptr() as usize, Ordering::SeqCst);
        ARFU_APPLIED.store(applied.as_ptr() as usize, Ordering::SeqCst);
        ARFU_APPLY_CALLS.store(0, Ordering::SeqCst);
        ctx.set_invoke_virtual_hook(relocating_arfu_apply);

        let result = arfu_update_with_operator(&mut ctx, target_old, slot, op_old, true);

        assert_eq!(result.unwrap(), Some(Value::Object(Some(applied))));
        assert_eq!(ARFU_APPLY_CALLS.load(Ordering::SeqCst), 1);
        assert_eq!(
            ctx.get_field(target_new, slot),
            Value::Object(Some(applied))
        );
        assert_eq!(
            ctx.get_field(target_old, slot),
            Value::Object(Some(prev_old))
        );
        assert_eq!(ctx.native_pin_count_for_test(), 0);
    }

    // -----------------------------------------------------------------
    // T19.H5: ARFU.newUpdater happy + error paths
    // -----------------------------------------------------------------

    #[test]
    fn t19_h5_register_natives_lists_every_factory() {
        let mut r = NativeMethodRegistry::new();
        register_atomic_updater_natives(&mut r);
        assert!(r
            .find(
                CLS_REF_FIELD_UPDATER,
                "newUpdater",
                "(Ljava/lang/Class;Ljava/lang/Class;Ljava/lang/String;)Ljava/util/concurrent/atomic/AtomicReferenceFieldUpdater;",
            )
            .is_some());
        assert!(r
            .find(
                CLS_INT_FIELD_UPDATER,
                "newUpdater",
                "(Ljava/lang/Class;Ljava/lang/String;)Ljava/util/concurrent/atomic/AtomicIntegerFieldUpdater;",
            )
            .is_some());
        assert!(r
            .find(
                CLS_LONG_FIELD_UPDATER,
                "newUpdater",
                "(Ljava/lang/Class;Ljava/lang/String;)Ljava/util/concurrent/atomic/AtomicLongFieldUpdater;",
            )
            .is_some());
        // Plus impl-class accessors must be present.
        assert!(r
            .find(
                CLS_REF_FIELD_UPDATER_IMPL,
                "compareAndSet",
                "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Z",
            )
            .is_some());
        assert!(r
            .find(
                CLS_INT_FIELD_UPDATER_IMPL,
                "getAndAdd",
                "(Ljava/lang/Object;I)I"
            )
            .is_some());
    }

    /// `--jdk-only` must register **nothing** from this module.
    ///
    /// Not just `newUpdater`. The base-class accessor rows are the half that
    /// fails silently: `getAndIncrement`/`addAndGet`/… are CONCRETE on the
    /// abstract `Atomic*FieldUpdater` in the JDK and are not overridden by
    /// `Atomic*FieldUpdaterImpl`, so with a real impl receiver CratonVM's
    /// superclass-walking virtual dispatch would hand the call to the native,
    /// `impl_slot` would read the impl's `cclass` reference out of
    /// `FU_SLOT_FIELD_INDEX`, get `None`, and `.unwrap_or(0)` would fetch-add
    /// **slot 0** of the target object. A test that only checked `newUpdater`
    /// would be green on precisely the tree that does that.
    ///
    /// Asserted as "the whole registry is empty" as well as per triple, so a
    /// future registration added inside `register_arfu`/`register_aifu`/
    /// `register_alfu` is covered without anyone remembering to extend the
    /// list. The compatible arm below is the mutation control: it proves the
    /// emptiness is the mode's doing and not a broken registrar.
    #[test]
    fn t19_h5_jdk_only_registers_nothing_from_this_module() {
        let mut strict = NativeMethodRegistry::new();
        strict.set_compatibility_mode(cratonvm_types::compat::CompatibilityMode::JdkOnly);
        register_atomic_updater_natives(&mut strict);
        assert!(
            strict.is_empty(),
            "--jdk-only must register no *FieldUpdater native at all ({} rows present); \
             the JDK's own Atomic*FieldUpdaterImpl bytecode is what strict mode exists \
             to run",
            strict.len()
        );

        for (cls, name, desc) in [
            (
                CLS_REF_FIELD_UPDATER,
                "newUpdater",
                "(Ljava/lang/Class;Ljava/lang/Class;Ljava/lang/String;)Ljava/util/concurrent/atomic/AtomicReferenceFieldUpdater;",
            ),
            (
                CLS_INT_FIELD_UPDATER,
                "newUpdater",
                "(Ljava/lang/Class;Ljava/lang/String;)Ljava/util/concurrent/atomic/AtomicIntegerFieldUpdater;",
            ),
            (
                CLS_LONG_FIELD_UPDATER,
                "newUpdater",
                "(Ljava/lang/Class;Ljava/lang/String;)Ljava/util/concurrent/atomic/AtomicLongFieldUpdater;",
            ),
            // The silent half — the accessors, which is where a `newUpdater`-only
            // test would be green on a broken tree.
            //
            // These used to be the ABSTRACT BASE classes. They are the impl
            // classes since 2026-08-27: registering an accessor on
            // `AtomicIntegerFieldUpdater` itself shadowed the inherited defaults
            // of any application subclass of that public abstract class, and the
            // rows were dead for this module's own objects anyway (dispatch
            // probes the receiver's own class first). See the note above
            // `register_aifu`'s impl block for the measurement.
            //
            // This list has to name registrations that really exist, because the
            // compatible arm below is a MUTATION CONTROL: if the triple is wrong
            // the strict assertion above passes vacuously. When the base rows
            // were removed, this control is what failed and said so.
            (CLS_REF_FIELD_UPDATER_IMPL, "get", "(Ljava/lang/Object;)Ljava/lang/Object;"),
            (CLS_INT_FIELD_UPDATER_IMPL, "getAndIncrement", "(Ljava/lang/Object;)I"),
            (CLS_INT_FIELD_UPDATER_IMPL, "addAndGet", "(Ljava/lang/Object;I)I"),
            (CLS_LONG_FIELD_UPDATER_IMPL, "incrementAndGet", "(Ljava/lang/Object;)J"),
            (CLS_LONG_FIELD_UPDATER_IMPL, "getAndAdd", "(Ljava/lang/Object;J)J"),
        ] {
            assert!(
                strict.find(cls, name, desc).is_none(),
                "--jdk-only still serves {cls}.{name}{desc}"
            );
            // The control: the same triple IS served in Compatible mode, so a
            // green assertion above cannot come from a typo in the triple.
            let mut compatible = NativeMethodRegistry::new();
            register_atomic_updater_natives(&mut compatible);
            assert!(
                compatible.find(cls, name, desc).is_some(),
                "{cls}.{name}{desc} is not registered in Compatible mode either — the \
                 strict assertion above is vacuous"
            );
        }
    }

    #[test]
    fn t19_h5_arfu_new_updater_happy_path_returns_populated_impl() {
        let mut um = UpdaterMock::new();
        let (tmirror, tcid) = make_class_mirror_um(&mut um, "ExtHandler");
        let (vmirror, vcid) = make_class_mirror_um(&mut um, "java/util/logging/Handler");
        um.add_field(
            tcid,
            "next",
            "Ljava/util/logging/Handler;",
            0x0040, /* ACC_VOLATILE */
            7,
            false,
        );
        let name = um.create_string("next");
        let result = native_arfu_new_updater(
            &mut um,
            &[
                Value::Object(Some(tmirror)),
                Value::Object(Some(vmirror)),
                Value::Object(Some(name)),
            ],
        )
        .unwrap()
        .unwrap();
        let impl_obj = match result {
            Value::Object(Some(o)) => o,
            other => panic!("expected ObjectRef, got {other:?}"),
        };
        // tclass / slot / desc-tag / vclass slots populated.
        assert_eq!(
            um.get_field(impl_obj, FU_SLOT_TCLASS_ID),
            Value::Int(tcid.as_u32() as i32),
        );
        assert_eq!(um.get_field(impl_obj, FU_SLOT_FIELD_INDEX), Value::Int(7));
        assert_eq!(
            um.get_field(impl_obj, FU_SLOT_DESC_TAG),
            Value::Int(DESC_TAG_REF)
        );
        assert_eq!(
            um.get_field(impl_obj, FU_SLOT_VCLASS_ID),
            Value::Int(vcid.as_u32() as i32),
        );
    }

    #[test]
    fn t19_h5_arfu_new_updater_missing_field_throws_runtime_exception() {
        let mut um = UpdaterMock::new();
        let (tmirror, _) = make_class_mirror_um(&mut um, "Target");
        let (vmirror, _) = make_class_mirror_um(&mut um, "java/lang/Object");
        let name = um.create_string("does_not_exist");
        let err = native_arfu_new_updater(
            &mut um,
            &[
                Value::Object(Some(tmirror)),
                Value::Object(Some(vmirror)),
                Value::Object(Some(name)),
            ],
        )
        .unwrap_err();
        // A THROWN java.lang.RuntimeException, not an IllegalArgumentException.
        //
        // CHANGED 2026-08-27, and the old assertion was this VM's behaviour
        // rather than the JDK's. All three `newUpdater` factories wrap their
        // reflection in `catch (Exception ex) { throw new RuntimeException(ex); }`,
        // so a missing field arrives as a plain `RuntimeException`.
        // MEASURED (`probes/AtomicUpdaterSweep.java`): HotSpot
        // `java.lang.RuntimeException`, this VM `IllegalArgumentException`.
        //
        // The mock raises it through `new_object_initialized`, so it lands as
        // `ExceptionThrown` rather than an `InternalError` variant -- which is
        // itself the signal that the refusal now goes out as a real Java
        // throwable of a named class instead of a Rust-side enum arm.
        assert!(
            matches!(err, MethodCallFailed::ExceptionThrown(_)),
            "a missing field must be a thrown java.lang.RuntimeException, got {err:?}"
        );
    }

    #[test]
    fn t19_h5_arfu_new_updater_bad_field_name_throws_iae() {
        let mut um = UpdaterMock::new();
        let (tmirror, _) = make_class_mirror_um(&mut um, "Target");
        let (vmirror, _) = make_class_mirror_um(&mut um, "java/lang/Object");
        let name = um.create_string("../traversal");
        let err = native_arfu_new_updater(
            &mut um,
            &[
                Value::Object(Some(tmirror)),
                Value::Object(Some(vmirror)),
                Value::Object(Some(name)),
            ],
        )
        .unwrap_err();
        assert!(matches!(
            err,
            MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
                RuntimeError::IllegalArgumentException { .. }
            ))
        ));
    }

    #[test]
    fn t19_h5_arfu_new_updater_type_mismatch_throws_cce() {
        let mut um = UpdaterMock::new();
        let (tmirror, tcid) = make_class_mirror_um(&mut um, "Target");
        let (vmirror_wrong, _) = make_class_mirror_um(&mut um, "java/lang/Integer");
        // declared as `String` but vclass is `Integer` — type mismatch.
        // ACC_VOLATILE: this test is about the vclass MISMATCH, and a
        // non-volatile fixture would now fail one check earlier for an
        // unrelated reason.
        um.add_field(tcid, "f", "Ljava/lang/String;", 0x0040, 0, false);
        let name = um.create_string("f");
        let err = native_arfu_new_updater(
            &mut um,
            &[
                Value::Object(Some(tmirror)),
                Value::Object(Some(vmirror_wrong)),
                Value::Object(Some(name)),
            ],
        )
        .unwrap_err();
        match err {
            MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
                RuntimeError::ClassCastException { .. },
            )) => {}
            other => panic!("expected ClassCastException, got {other:?}"),
        }
    }

    #[test]
    fn t19_h5_arfu_new_updater_object_declared_accepts_any_vclass() {
        // A field declared as `java/lang/Object` accepts any vclass —
        // this matches the JDK's special-case for the universal type.
        let mut um = UpdaterMock::new();
        let (tmirror, tcid) = make_class_mirror_um(&mut um, "Target");
        let (vmirror, _) = make_class_mirror_um(&mut um, "java/lang/Integer");
        // ACC_VOLATILE — see the sibling test above.
        um.add_field(tcid, "f", "Ljava/lang/Object;", 0x0040, 0, false);
        let name = um.create_string("f");
        let res = native_arfu_new_updater(
            &mut um,
            &[
                Value::Object(Some(tmirror)),
                Value::Object(Some(vmirror)),
                Value::Object(Some(name)),
            ],
        );
        assert!(res.is_ok());
    }

    #[test]
    fn t19_h5_arfu_new_updater_static_field_throws_iae() {
        let mut um = UpdaterMock::new();
        let (tmirror, tcid) = make_class_mirror_um(&mut um, "Target");
        let (vmirror, _) = make_class_mirror_um(&mut um, "java/lang/Object");
        um.add_field(tcid, "stat", "Ljava/lang/Object;", 0x0008, 0, true);
        let name = um.create_string("stat");
        let err = native_arfu_new_updater(
            &mut um,
            &[
                Value::Object(Some(tmirror)),
                Value::Object(Some(vmirror)),
                Value::Object(Some(name)),
            ],
        )
        .unwrap_err();
        assert!(matches!(
            err,
            MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
                RuntimeError::IllegalArgumentException { .. }
            ))
        ));
    }

    #[test]
    fn t19_h5_arfu_new_updater_final_field_throws_iae() {
        let mut um = UpdaterMock::new();
        let (tmirror, tcid) = make_class_mirror_um(&mut um, "Target");
        let (vmirror, _) = make_class_mirror_um(&mut um, "java/lang/Object");
        um.add_field(tcid, "fin", "Ljava/lang/Object;", 0x0010, 0, false);
        let name = um.create_string("fin");
        let err = native_arfu_new_updater(
            &mut um,
            &[
                Value::Object(Some(tmirror)),
                Value::Object(Some(vmirror)),
                Value::Object(Some(name)),
            ],
        )
        .unwrap_err();
        assert!(matches!(
            err,
            MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
                RuntimeError::IllegalArgumentException { .. }
            ))
        ));
    }

    // -----------------------------------------------------------------
    // T19.H5: AIFU / ALFU happy + error paths
    // -----------------------------------------------------------------

    #[test]
    fn t19_h5_aifu_new_updater_happy_path_int_field() {
        let mut um = UpdaterMock::new();
        let (tmirror, tcid) = make_class_mirror_um(&mut um, "Target");
        um.add_field(tcid, "counter", "I", 0x0040, 3, false);
        let name = um.create_string("counter");
        let res = native_aifu_new_updater(
            &mut um,
            &[Value::Object(Some(tmirror)), Value::Object(Some(name))],
        )
        .unwrap()
        .unwrap();
        let impl_obj = match res {
            Value::Object(Some(o)) => o,
            _ => panic!(),
        };
        assert_eq!(um.get_field(impl_obj, FU_SLOT_FIELD_INDEX), Value::Int(3));
        assert_eq!(
            um.get_field(impl_obj, FU_SLOT_DESC_TAG),
            Value::Int(DESC_TAG_INT)
        );
    }

    #[test]
    fn t19_h5_aifu_new_updater_long_field_rejected() {
        let mut um = UpdaterMock::new();
        let (tmirror, tcid) = make_class_mirror_um(&mut um, "Target");
        // ACC_VOLATILE, and this one MATTERS: the assertion below only checks
        // for `IllegalArgumentException`, and "field must be volatile" is also
        // an IAE. With a non-volatile fixture this test went green for a
        // reason that has nothing to do with the long/int descriptor split it
        // is named for.
        um.add_field(tcid, "v", "J", 0x0040, 0, false);
        let name = um.create_string("v");
        let err = native_aifu_new_updater(
            &mut um,
            &[Value::Object(Some(tmirror)), Value::Object(Some(name))],
        )
        .unwrap_err();
        assert!(matches!(
            err,
            MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
                RuntimeError::IllegalArgumentException { .. }
            ))
        ));
    }

    #[test]
    fn t19_h5_alfu_new_updater_happy_path_long_field() {
        let mut um = UpdaterMock::new();
        let (tmirror, tcid) = make_class_mirror_um(&mut um, "Target");
        um.add_field(tcid, "ts", "J", 0x0040, 5, false);
        let name = um.create_string("ts");
        let res = native_alfu_new_updater(
            &mut um,
            &[Value::Object(Some(tmirror)), Value::Object(Some(name))],
        )
        .unwrap()
        .unwrap();
        let impl_obj = match res {
            Value::Object(Some(o)) => o,
            _ => panic!(),
        };
        assert_eq!(um.get_field(impl_obj, FU_SLOT_FIELD_INDEX), Value::Int(5));
        assert_eq!(
            um.get_field(impl_obj, FU_SLOT_DESC_TAG),
            Value::Int(DESC_TAG_LONG)
        );
    }

    // -----------------------------------------------------------------
    // T19.H5: Accessor round-trips (compareAndSet / getAndSet / get / set)
    // -----------------------------------------------------------------

    #[test]
    fn t19_h5_arfu_compare_and_set_round_trip() {
        let mut um = UpdaterMock::new();
        let (tmirror, tcid) = make_class_mirror_um(&mut um, "Target");
        let (vmirror, _) = make_class_mirror_um(&mut um, "java/lang/Object");
        um.add_field(tcid, "next", "Ljava/lang/Object;", 0x0040, 1, false);
        let name = um.create_string("next");
        let updater = match native_arfu_new_updater(
            &mut um,
            &[
                Value::Object(Some(tmirror)),
                Value::Object(Some(vmirror)),
                Value::Object(Some(name)),
            ],
        )
        .unwrap()
        .unwrap()
        {
            Value::Object(Some(o)) => o,
            _ => panic!(),
        };
        // Allocate a fake Target instance with 4 slots (slot 1 is the field).
        let target = um.alloc_object(tcid, 4);
        // Initially Object(None) (default).  CAS from None → some value.
        let some_value_obj = um.create_string("hello");
        let ok = native_arfu_compare_and_set(
            &mut um,
            &[
                Value::Object(Some(updater)),
                Value::Object(Some(target)),
                Value::Int(0), // expected: not Object(None) — see below
                Value::Object(Some(some_value_obj)),
            ],
        )
        .unwrap();
        // Default field for slot 1 in MockNativeContext is Value::Int(0),
        // so the CAS expected=Int(0) succeeds and writes the new value.
        assert_eq!(ok, Some(Value::Int(1)));
        let post = native_arfu_get(
            &mut um,
            &[Value::Object(Some(updater)), Value::Object(Some(target))],
        )
        .unwrap()
        .unwrap();
        match post {
            Value::Object(Some(s)) => {
                assert_eq!(um.read_string(s).as_deref(), Some("hello"));
            }
            other => panic!("expected new string, got {other:?}"),
        }
    }

    #[test]
    fn t19_h5_aifu_get_and_add_round_trip() {
        let mut um = UpdaterMock::new();
        let (tmirror, tcid) = make_class_mirror_um(&mut um, "Counter");
        um.add_field(tcid, "n", "I", 0x0040, 0, false);
        let name = um.create_string("n");
        let updater = match native_aifu_new_updater(
            &mut um,
            &[Value::Object(Some(tmirror)), Value::Object(Some(name))],
        )
        .unwrap()
        .unwrap()
        {
            Value::Object(Some(o)) => o,
            _ => panic!(),
        };
        let target = um.alloc_object(tcid, 1);
        // default slot 0 is Int(0); add 5 — result is current 0, slot now 5.
        let prev = native_aifu_get_and_add(
            &mut um,
            &[
                Value::Object(Some(updater)),
                Value::Object(Some(target)),
                Value::Int(5),
            ],
        )
        .unwrap()
        .unwrap();
        assert_eq!(prev, Value::Int(0));
        let after = native_aifu_get(
            &mut um,
            &[Value::Object(Some(updater)), Value::Object(Some(target))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(after, Value::Int(5));
    }

    #[test]
    fn t19_h5_alfu_set_and_get_round_trip() {
        let mut um = UpdaterMock::new();
        let (tmirror, tcid) = make_class_mirror_um(&mut um, "TimestampHolder");
        um.add_field(tcid, "ts", "J", 0x0040, 0, false);
        let name = um.create_string("ts");
        let updater = match native_alfu_new_updater(
            &mut um,
            &[Value::Object(Some(tmirror)), Value::Object(Some(name))],
        )
        .unwrap()
        .unwrap()
        {
            Value::Object(Some(o)) => o,
            _ => panic!(),
        };
        let target = um.alloc_object(tcid, 1);
        native_alfu_set(
            &mut um,
            &[
                Value::Object(Some(updater)),
                Value::Object(Some(target)),
                Value::Long(1234567890123),
            ],
        )
        .unwrap();
        let got = native_alfu_get(
            &mut um,
            &[Value::Object(Some(updater)), Value::Object(Some(target))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(got, Value::Long(1234567890123));
    }

    #[test]
    fn t19_h5_aifu_compare_and_set_observable_progress() {
        // Bench-style: 4 sequential increments must produce monotonically
        // increasing observed values.  Simulates the multi-thread case
        // without spawning real threads (UpdaterMock is single-threaded).
        let mut um = UpdaterMock::new();
        let (tmirror, tcid) = make_class_mirror_um(&mut um, "Counter");
        um.add_field(tcid, "n", "I", 0x0040, 0, false);
        let name = um.create_string("n");
        let updater = match native_aifu_new_updater(
            &mut um,
            &[Value::Object(Some(tmirror)), Value::Object(Some(name))],
        )
        .unwrap()
        .unwrap()
        {
            Value::Object(Some(o)) => o,
            _ => panic!(),
        };
        let target = um.alloc_object(tcid, 1);
        for expected in 1..=4 {
            let r = native_aifu_increment_and_get(
                &mut um,
                &[Value::Object(Some(updater)), Value::Object(Some(target))],
            )
            .unwrap()
            .unwrap();
            assert_eq!(r, Value::Int(expected));
        }
    }

    #[test]
    fn t19_h5_arfu_get_and_set_replaces_value() {
        let mut um = UpdaterMock::new();
        let (tmirror, tcid) = make_class_mirror_um(&mut um, "Holder");
        let (vmirror, _) = make_class_mirror_um(&mut um, "java/lang/Object");
        um.add_field(tcid, "ref", "Ljava/lang/Object;", 0x0040, 0, false);
        let name = um.create_string("ref");
        let updater = match native_arfu_new_updater(
            &mut um,
            &[
                Value::Object(Some(tmirror)),
                Value::Object(Some(vmirror)),
                Value::Object(Some(name)),
            ],
        )
        .unwrap()
        .unwrap()
        {
            Value::Object(Some(o)) => o,
            _ => panic!(),
        };
        let target = um.alloc_object(tcid, 1);
        // Pre-populate slot 0 with a marker ref.
        let first = um.create_string("first");
        um.set_field(target, 0, Value::Object(Some(first)));
        let next = um.create_string("second");
        let prev = native_arfu_get_and_set(
            &mut um,
            &[
                Value::Object(Some(updater)),
                Value::Object(Some(target)),
                Value::Object(Some(next)),
            ],
        )
        .unwrap()
        .unwrap();
        match prev {
            Value::Object(Some(s)) => {
                assert_eq!(um.read_string(s).as_deref(), Some("first"));
            }
            _ => panic!(),
        }
        match um.get_field(target, 0) {
            Value::Object(Some(s)) => {
                assert_eq!(um.read_string(s).as_deref(), Some("second"));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn t19_h5_null_target_returns_cce_not_silent() {
        let mut um = UpdaterMock::new();
        let (tmirror, tcid) = make_class_mirror_um(&mut um, "Target");
        let (vmirror, _) = make_class_mirror_um(&mut um, "java/lang/Object");
        um.add_field(tcid, "f", "Ljava/lang/Object;", 0x0040, 0, false);
        let name = um.create_string("f");
        let updater = match native_arfu_new_updater(
            &mut um,
            &[
                Value::Object(Some(tmirror)),
                Value::Object(Some(vmirror)),
                Value::Object(Some(name)),
            ],
        )
        .unwrap()
        .unwrap()
        {
            Value::Object(Some(o)) => o,
            _ => panic!(),
        };
        // Pass null as target — must throw, and specifically a
        // ClassCastException.
        //
        // CHANGED 2026-08-27, from NullPointerException. This test's NAME is
        // still exactly right — the point is that a null target is not silently
        // absorbed — but the TYPE it asserted was this VM's, not the JDK's.
        //
        // MEASURED against HotSpot 25.0.3+9 (`probes/AtomicUpdaterSweep.java`):
        //
        //   u.get(null)   HotSpot ClassCastException   CratonVM (then) NPE
        //
        // The JDK's accessors open with `if (!tclass.isInstance(obj)) throw new
        // ClassCastException()`, and `isInstance` is FALSE for null — so a null
        // target leaves by the same door as a wrong-typed one, and never
        // reaches a dereference. A caller catching CCE around a field-updater
        // access is catching the documented type; NPE escaped it.
        let err = native_arfu_get(
            &mut um,
            &[Value::Object(Some(updater)), Value::Object(None)],
        )
        .unwrap_err();
        assert!(
            matches!(
                err,
                MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
                    RuntimeError::ClassCastException { .. }
                ))
            ),
            "null target must be a ClassCastException like the JDK's, got {err:?}"
        );
    }

    #[test]
    fn t19_h5_descriptor_to_internal_name_only_for_pure_class_descriptor() {
        assert_eq!(
            ref_descriptor_to_internal_name("Ljava/lang/Object;"),
            Some("java/lang/Object".to_string())
        );
        assert_eq!(ref_descriptor_to_internal_name("[I"), None);
        assert_eq!(ref_descriptor_to_internal_name("I"), None);
    }

    // -----------------------------------------------------------------
    // getAndIncrement / getAndDecrement / addAndGet return semantics.
    //
    // These accessors previously used a bounded CAS loop that, on
    // contended loop exhaustion, returned a fabricated `0` and applied
    // NO update. They now delegate to the unbounded `atomic_fetch_add_*`
    // primitive. getAnd* must return the OLD value (and bump the slot);
    // addAndGet must return the NEW value. The tests assert both the
    // return value and the committed slot value so a regression to the
    // bogus-0 fallthrough (which left the slot unchanged) would fail.
    // -----------------------------------------------------------------

    /// Build an int updater over field `n` (slot 0) of a fresh `Counter`
    /// instance and return `(um, updater, target)`.
    fn int_updater_over_slot0(start: i32) -> (UpdaterMock, ObjectRef, ObjectRef) {
        let mut um = UpdaterMock::new();
        let (tmirror, tcid) = make_class_mirror_um(&mut um, "Counter");
        um.add_field(tcid, "n", "I", 0x0040, 0, false);
        let name = um.create_string("n");
        let updater = match native_aifu_new_updater(
            &mut um,
            &[Value::Object(Some(tmirror)), Value::Object(Some(name))],
        )
        .unwrap()
        .unwrap()
        {
            Value::Object(Some(o)) => o,
            _ => panic!(),
        };
        let target = um.alloc_object(tcid, 1);
        um.set_field(target, 0, Value::Int(start));
        (um, updater, target)
    }

    /// Build a long updater over field `v` (slot 0) of a fresh `LongHolder`.
    fn long_updater_over_slot0(start: i64) -> (UpdaterMock, ObjectRef, ObjectRef) {
        let mut um = UpdaterMock::new();
        let (tmirror, tcid) = make_class_mirror_um(&mut um, "LongHolder");
        um.add_field(tcid, "v", "J", 0x0040, 0, false);
        let name = um.create_string("v");
        let updater = match native_alfu_new_updater(
            &mut um,
            &[Value::Object(Some(tmirror)), Value::Object(Some(name))],
        )
        .unwrap()
        .unwrap()
        {
            Value::Object(Some(o)) => o,
            _ => panic!(),
        };
        let target = um.alloc_object(tcid, 1);
        um.set_field(target, 0, Value::Long(start));
        (um, updater, target)
    }

    #[test]
    fn t19_h5_aifu_get_and_increment_returns_old_and_bumps_slot() {
        let (mut um, updater, target) = int_updater_over_slot0(41);
        let ret = native_aifu_get_and_increment(
            &mut um,
            &[Value::Object(Some(updater)), Value::Object(Some(target))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(ret, Value::Int(41), "getAndIncrement returns the OLD value");
        assert_eq!(um.get_field(target, 0), Value::Int(42), "slot is bumped");
    }

    #[test]
    fn t19_h5_aifu_get_and_decrement_returns_old_and_bumps_slot() {
        let (mut um, updater, target) = int_updater_over_slot0(7);
        let ret = native_aifu_get_and_decrement(
            &mut um,
            &[Value::Object(Some(updater)), Value::Object(Some(target))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(ret, Value::Int(7), "getAndDecrement returns the OLD value");
        assert_eq!(
            um.get_field(target, 0),
            Value::Int(6),
            "slot is decremented"
        );
    }

    #[test]
    fn t19_h5_aifu_add_and_get_returns_new_value() {
        let (mut um, updater, target) = int_updater_over_slot0(100);
        let ret = native_aifu_add_and_get(
            &mut um,
            &[
                Value::Object(Some(updater)),
                Value::Object(Some(target)),
                Value::Int(-30),
            ],
        )
        .unwrap()
        .unwrap();
        assert_eq!(ret, Value::Int(70), "addAndGet returns the NEW value");
        assert_eq!(
            um.get_field(target, 0),
            Value::Int(70),
            "slot holds new value"
        );
    }

    #[test]
    fn t19_h5_alfu_get_and_increment_returns_old_and_bumps_slot() {
        let (mut um, updater, target) = long_updater_over_slot0(9_000_000_000);
        let ret = native_alfu_get_and_increment(
            &mut um,
            &[Value::Object(Some(updater)), Value::Object(Some(target))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            ret,
            Value::Long(9_000_000_000),
            "getAndIncrement returns the OLD value"
        );
        assert_eq!(um.get_field(target, 0), Value::Long(9_000_000_001));
    }

    #[test]
    fn t19_h5_alfu_get_and_decrement_returns_old_and_bumps_slot() {
        let (mut um, updater, target) = long_updater_over_slot0(1);
        let ret = native_alfu_get_and_decrement(
            &mut um,
            &[Value::Object(Some(updater)), Value::Object(Some(target))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(ret, Value::Long(1), "getAndDecrement returns the OLD value");
        assert_eq!(um.get_field(target, 0), Value::Long(0));
    }

    #[test]
    fn t19_h5_alfu_add_and_get_returns_new_value() {
        let (mut um, updater, target) = long_updater_over_slot0(1_000);
        let ret = native_alfu_add_and_get(
            &mut um,
            &[
                Value::Object(Some(updater)),
                Value::Object(Some(target)),
                Value::Long(250),
            ],
        )
        .unwrap()
        .unwrap();
        assert_eq!(ret, Value::Long(1_250), "addAndGet returns the NEW value");
        assert_eq!(um.get_field(target, 0), Value::Long(1_250));
    }

    #[test]
    fn t19_h5_int_accessors_accumulate_across_calls() {
        // Sequential calls must accumulate (no fabricated-0 reset): the
        // old bounded-loop fallthrough would have lost updates and broken
        // this monotonic progression under contention.
        let (mut um, updater, target) = int_updater_over_slot0(0);
        for i in 0..5 {
            let old = native_aifu_get_and_increment(
                &mut um,
                &[Value::Object(Some(updater)), Value::Object(Some(target))],
            )
            .unwrap()
            .unwrap();
            assert_eq!(
                old,
                Value::Int(i),
                "each getAndIncrement returns prior count"
            );
        }
        assert_eq!(um.get_field(target, 0), Value::Int(5));
    }
}
