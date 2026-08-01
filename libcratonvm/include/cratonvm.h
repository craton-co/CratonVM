/* SPDX-License-Identifier: Apache-2.0
 * Copyright 2024-2026 Craton Software Company
 *
 * cratonvm.h — public C header for the libcratonvm flat embedding API
 * (Layer 2 of docs/feature-designs/embedding-api.md).
 *
 * STATUS: checked-in public header for the flat C ABI. The declarations here
 * match the `#[repr(C)]` / `#[no_mangle] pub extern "C"` flat API items in
 * libcratonvm/src/lib.rs. cbindgen regeneration is opt-in; when regenerated,
 * keep the ABI coverage aligned with this file (handle types, CratonValue
 * layout, constants, and function signatures).
 *
 * This header covers ONLY the flat `cratonvm_*` API. The JNI Invocation-API
 * entry points (JNI_CreateJavaVM / JNI_GetDefaultJavaVMInitArgs /
 * JNI_GetCreatedJavaVMs) are the standard jni.h surface and are intentionally
 * not redeclared here — include a JDK <jni.h> for those.
 */

#ifndef CRATONVM_H
#define CRATONVM_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* ---- integer / handle types ------------------------------------------- */

typedef int32_t cratonvm_jint;
typedef uint8_t cratonvm_jboolean;

/* Opaque, owning VM handle from cratonvm_create(); free with cratonvm_destroy(). */
typedef struct CratonVm CratonVm;

/* Opaque u64 object/string/throwable token (0 == null). */
typedef uint64_t CratonRef;

/* u64 class handle (a widened ClassId). 0 is a valid class id. */
typedef uint64_t CratonClass;

/* JNI-compatible return codes and default version used by this flat API.
 * Prefixed names avoid collisions when a host also includes a JDK <jni.h>. */
enum {
    CRATONVM_JNI_OK       = 0,
    CRATONVM_JNI_ERR      = -1,
    CRATONVM_JNI_EDETACHED = -2,
    CRATONVM_JNI_EVERSION = -3,
    CRATONVM_JNI_ENOMEM   = -4,
    CRATONVM_JNI_EEXIST   = -5,
    CRATONVM_JNI_EINVAL   = -6,
    CRATONVM_JNI_VERSION  = 0x00010008
};

/* ---- VM init args (jni.h-compatible; reused by cratonvm_create) -------- */

typedef struct JavaVMOption {
    char *optionString;
    void *extraInfo;
} JavaVMOption;

typedef struct JavaVMInitArgs {
    cratonvm_jint     version;
    cratonvm_jint     nOptions;
    JavaVMOption     *options;
    cratonvm_jboolean ignoreUnrecognized;
} JavaVMInitArgs;

/* ---- tagged value ----------------------------------------------------- */

/* CratonValue.tag discriminants. */
enum {
    CRATON_TAG_VOID   = 0,   /* no value / void return / empty slot */
    CRATON_TAG_INT    = 1,   /* int32 (also boolean/byte/char/short) */
    CRATON_TAG_LONG   = 2,   /* int64 */
    CRATON_TAG_FLOAT  = 3,   /* float bits in low 32 of payload */
    CRATON_TAG_DOUBLE = 4,   /* double bits in payload */
    CRATON_TAG_OBJECT = 5,   /* payload is a CratonRef */
    CRATON_TAG_ERROR  = -1   /* call failed; see cratonvm_last_error() */
};

/* C-ABI tagged value exchanged with the flat API.
 *   tag == INT    -> (int32_t)payload     (sign-extended on read)
 *   tag == LONG   -> (int64_t)payload
 *   tag == FLOAT  -> bit-cast low 32 bits of payload to float
 *   tag == DOUBLE -> bit-cast payload to double
 *   tag == OBJECT -> (CratonRef)payload
 *   tag == VOID / ERROR -> payload unspecified
 *
 * Inbound values passed as method args or field values must use INT, LONG,
 * FLOAT, DOUBLE, or OBJECT. Unknown tags and stale/fabricated nonzero OBJECT
 * tokens fail the call and set cratonvm_last_error(); they are not coerced to
 * null.
 */
typedef struct CratonValue {
    cratonvm_jint tag;
    uint64_t      payload;
} CratonValue;

/* ---- compatibility mode ----------------------------------------------- */

/* Which substitutions the VM may make. ORTHOGONAL to the JDK mode
 * (--real-jdk / --synthetic-jdk), which selects which class library boots.
 *
 * COMPATIBLE is today's behaviour (bridges, intrinsics AND compatibility
 * shims) and is the default on every entry point — you reach it by doing
 * nothing. JDK_ONLY makes real JDK class bytes authoritative: no class is
 * fabricated without real bytes and no synthetic-stub native is registered or
 * invoked; it requires a real JDK runtime image.
 *
 * These numeric values are a PUBLISHED, STABLE, APPEND-ONLY part of the ABI: a
 * compiled host carries them in its .text, so a value may be added but never
 * renumbered. They are cratonvm_jint, deliberately NOT cratonvm_jboolean, so a
 * third enforcement posture can be added later without breaking a host that
 * was compiled against this header.
 *
 * JDK-only is an internal diagnostic, not a production posture: it is at stage
 * 1 of 4 of its rollout, so a host that enables it should expect failures on
 * programs that run fine under COMPATIBLE. See docs/EMBEDDING.md ("Choosing a
 * compatibility mode"). */
enum {
    CRATONVM_COMPATIBILITY_COMPATIBLE = 0,
    CRATONVM_COMPATIBILITY_JDK_ONLY   = 1
};

/* ---- lifecycle -------------------------------------------------------- */

/* Build and bootstrap a VM. `args` may be NULL for defaults. Only one VM
 * surface may be active in-process: either one JNI Invocation-API VM or one
 * flat CratonVm. Returns NULL on failure (cratonvm_last_error() then holds the
 * message). Unsupported JNI versions and unrecognized/malformed options fail
 * unless ignoreUnrecognized is nonzero. The returned handle must be released
 * with cratonvm_destroy(). */
CratonVm *cratonvm_create(const JavaVMInitArgs *args);

/* cratonvm_create() with the compatibility mode stated as an explicit
 * CRATONVM_COMPATIBILITY_* value instead of an option string — a C host has no
 * command line, and JavaVMInitArgs is often assembled far from the call site
 * that knows the policy. Everything else is identical to cratonvm_create().
 *
 * `compatibility_mode` must be a CRATONVM_COMPATIBILITY_* value. Any other
 * integer is REJECTED (NULL + cratonvm_last_error()), never clamped to
 * COMPATIBLE: a host built against a newer header is told, rather than
 * silently getting the loose policy it opted out of. Probe first with
 * cratonvm_compatibility_mode_supported() to avoid a failed create.
 *
 * The option string "--jdk-only" (spelled exactly as the launcher flag) is the
 * second route to strict mode, and the only one available to
 * JNI_CreateJavaVM(). Passing CRATONVM_COMPATIBILITY_JDK_ONLY alongside
 * --jdk-only agrees and is accepted; passing CRATONVM_COMPATIBILITY_COMPATIBLE
 * alongside --jdk-only is a CONTRADICTION error, not a precedence rule. */
CratonVm *cratonvm_create_with_compatibility(const JavaVMInitArgs *args,
                                             cratonvm_jint compatibility_mode);

/* Drop the VM and free the handle. NULL is a no-op. */
void cratonvm_destroy(CratonVm *vm);

/* Release one object/string/throwable token previously returned by this VM.
 * Passing 0 (Java null) is a no-op success. Each nonzero CratonRef returned by
 * libcratonvm pins a JNI global ref until the host balances it with this call
 * or destroys the VM. Returns 0 (JNI_OK) on success or -1 (JNI_ERR) for a bad
 * VM handle or stale/fabricated token. */
cratonvm_jint cratonvm_release_ref(CratonVm *vm, CratonRef reference);

/* Mark the calling VM-driving host thread as parked in native code while it
 * performs a blocking host wait, then re-enter the VM. Balance every enter with
 * one leave. Both return 0 (JNI_OK) on success or -1 (JNI_ERR) when no VM is
 * available. */
cratonvm_jint cratonvm_thread_enter_native(void);
cratonvm_jint cratonvm_thread_leave_native(void);

/* ---- operations ------------------------------------------------------- */

/* Load (and link) a class by internal name ("java/lang/System"). Writes the
 * resolved class handle into *out_class. Returns 0 (JNI_OK) on success or -1
 * (JNI_ERR) on failure; on failure the last error is set and *out_class is
 * left untouched. (Out-pointer + return code, not a sentinel handle, because
 * 0 is a valid ClassId.) */
cratonvm_jint cratonvm_load_class(CratonVm *vm, const char *name, CratonClass *out_class);

/* Invoke a static method by class/method name + JVM descriptor `sig`. `args`
 * is a typed CratonValue array of length `n_args` (may be NULL when
 * n_args == 0). Returns the result (tag VOID for a void method), or a value
 * with tag CRATON_TAG_ERROR on failure (see cratonvm_last_error()). */
CratonValue cratonvm_invoke_static(CratonVm *vm, const char *cls,
                                   const char *method, const char *sig,
                                   const CratonValue *args, cratonvm_jint n_args);

/* Invoke an instance method on `receiver` via VIRTUAL dispatch (resolved
 * against the receiver's runtime class). `sig` EXCLUDES the receiver
 * (e.g. "(I)Ljava/lang/String;"); the receiver is passed via `receiver`, not in
 * `args`. `args` is a typed CratonValue array of length `n_args` (NULL when 0).
 * Returns the result (tag VOID for void), or tag CRATON_TAG_ERROR on failure. */
CratonValue cratonvm_invoke_virtual(CratonVm *vm, CratonRef receiver,
                                    const char *method, const char *sig,
                                    const CratonValue *args, cratonvm_jint n_args);

/* Create an interned java.lang.String from a UTF-8 C string. Returns its
 * handle, or 0 on failure (last error set). */
CratonRef cratonvm_new_string(CratonVm *vm, const char *utf8);

/* Read a java.lang.String handle (e.g. an OBJECT result from
 * cratonvm_invoke_static) into a freshly-allocated NUL-terminated UTF-8 C
 * string. Returns NULL on failure (bad handle / not a String; last error set).
 * The buffer is CALLER-owned and must be released with cratonvm_free_string. */
char *cratonvm_string_utf8(CratonVm *vm, CratonRef str);

/* Release a buffer returned by cratonvm_string_utf8. NULL is a no-op. */
void cratonvm_free_string(char *s);

/* ---- object inspection / field read-back ------------------------------ */

/* Write the runtime class handle of `obj` into *out_class. Returns 0 (JNI_OK)
 * or -1 (JNI_ERR) on a bad handle (last error set, *out_class untouched). */
cratonvm_jint cratonvm_object_class(CratonVm *vm, CratonRef obj, CratonClass *out_class);

/* Read a class handle's internal name ("java/lang/String") into a freshly
 * allocated NUL-terminated UTF-8 buffer (CALLER-owned; free with
 * cratonvm_free_string). Returns NULL on a bad/unresolvable handle. */
char *cratonvm_class_name(CratonVm *vm, CratonClass cls);

/* Return the number of instance-field slots in `obj`'s class (the valid index
 * range [0, count) for cratonvm_get_field), or -1 on a bad handle. */
cratonvm_jint cratonvm_field_count(CratonVm *vm, CratonRef obj);

/* Read instance field slot `index` of `obj` as a typed CratonValue. The slot is
 * resolved by LAYOUT INDEX (see cratonvm_field_index / cratonvm_get_field_by_name
 * for name-based access). Returns tag CRATON_TAG_ERROR on a bad handle or
 * out-of-range index (last error set). */
CratonValue cratonvm_get_field(CratonVm *vm, CratonRef obj, cratonvm_jint index);

/* Resolve an instance field NAME on `cls` to its layout slot index (walking the
 * superclass chain; most-derived declaration wins), written to *out_index.
 * Returns 0 (JNI_OK) or -1 (JNI_ERR) on an unloaded class / unknown field (last
 * error set, *out_index untouched). The index is usable with cratonvm_get_field
 * / cratonvm_set_field. */
cratonvm_jint cratonvm_field_index(CratonVm *vm, CratonClass cls, const char *name,
                                   cratonvm_jint *out_index);

/* Descriptor-disambiguated field resolution: like cratonvm_field_index, but a
 * non-null `descriptor` (a JVM type descriptor: "I", "Ljava/lang/String;",
 * "[J", ...) must also match. This addresses a SHADOWED super-class field that a
 * subclass re-declares with the same name (name-only resolution returns the
 * most-derived one; passing the super-class field's descriptor walks past the
 * shadow). A null `descriptor` is identical to cratonvm_field_index (name-only).
 * Returns 0 (JNI_OK) or -1 (JNI_ERR) when no field matches name + descriptor. */
cratonvm_jint cratonvm_field_index_desc(CratonVm *vm, CratonClass cls, const char *name,
                                        const char *descriptor, cratonvm_jint *out_index);

/* Read the named instance field of `obj` (resolved against obj's RUNTIME class)
 * as a typed CratonValue. tag CRATON_TAG_ERROR on a bad handle / unknown name. */
CratonValue cratonvm_get_field_by_name(CratonVm *vm, CratonRef obj, const char *name);

/* Write `value` into instance field slot `index` of `obj`. GC-barrier correct
 * (same pre/post barriers as the interpreter's putfield). No coercion — the
 * caller ensures the tag matches the field's declared type. Returns 0 (JNI_OK)
 * or -1 (JNI_ERR) on a bad handle / out-of-range index (last error set). */
cratonvm_jint cratonvm_set_field(CratonVm *vm, CratonRef obj, cratonvm_jint index,
                                 CratonValue value);

/* Write `value` into the named instance field of `obj` (resolved against obj's
 * RUNTIME class), GC-barrier correct. Returns 0 (JNI_OK) or -1 (JNI_ERR) on a
 * bad handle / unknown name (last error set). */
cratonvm_jint cratonvm_set_field_by_name(CratonVm *vm, CratonRef obj, const char *name,
                                         CratonValue value);

/* ---- compatibility mode read-back / capability probe ------------------ */

/* The compatibility mode a LIVE VM is actually running under, as a
 * CRATONVM_COMPATIBILITY_* value. Returns -1 (never a mode value) on a
 * null/invalid handle, with the reason in cratonvm_last_error(). Read back what
 * you got rather than trusting what you asked for. */
cratonvm_jint cratonvm_compatibility_mode(CratonVm *vm);

/* Capability probe — NEEDS NO VM, so it answers "does this build know the
 * mode?" without the trial-and-error of a failed create; combined with dlsym it
 * also covers libraries that predate the symbol entirely. Returns 1 when this
 * build understands and honours `mode`, -1 when `mode` is not a
 * CRATONVM_COMPATIBILITY_* value at all. 0 is reserved for a mode this build
 * knows but cannot honour (nothing returns it today). */
cratonvm_jint cratonvm_compatibility_mode_supported(cratonvm_jint mode);

/* ---- error access (thread-local) -------------------------------------- */

/* Return this thread's pending error message, or NULL if none. The pointer is
 * library-owned and valid until the next cratonvm_* call on this thread or a
 * cratonvm_clear_error(); copy it to retain. `vm` is accepted for symmetry but
 * not dereferenced (error state is thread-local). */
const char *cratonvm_last_error(CratonVm *vm);

/* Clear this thread's pending error. `vm` is ignored. */
void cratonvm_clear_error(CratonVm *vm);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* CRATONVM_H */
