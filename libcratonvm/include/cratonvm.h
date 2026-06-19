/* SPDX-License-Identifier: Apache-2.0
 * Copyright 2024-2026 Craton Software Company
 *
 * cratonvm.h — public C header for the libcratonvm flat embedding API
 * (Layer 2 of docs/feature-designs/embedding-api.md).
 *
 * STATUS: hand-written stub. The flat C ABI is small and stable enough to
 * maintain by hand for now; the design's intent is to generate this with
 * cbindgen as a build step (see "Next steps" in the design doc). The
 * declarations here match the `#[repr(C)]` / `#[no_mangle] pub extern "C"`
 * items in libcratonvm/src/lib.rs exactly. If you regenerate with cbindgen,
 * keep this file's contents byte-compatible (handle types, CratonValue layout,
 * tag values, function signatures).
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

/* u64 object/string/throwable handle (ObjectRef::as_ptr(); 0 == null). */
typedef uint64_t CratonRef;

/* u64 class handle (a widened ClassId). 0 is a valid class id. */
typedef uint64_t CratonClass;

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
 */
typedef struct CratonValue {
    cratonvm_jint tag;
    uint64_t      payload;
} CratonValue;

/* ---- lifecycle -------------------------------------------------------- */

/* Build and bootstrap a VM. `args` may be NULL for defaults. Returns NULL on
 * failure (cratonvm_last_error() then holds the message). The returned handle
 * must be released with cratonvm_destroy(). */
CratonVm *cratonvm_create(const JavaVMInitArgs *args);

/* Drop the VM and free the handle. NULL is a no-op. */
void cratonvm_destroy(CratonVm *vm);

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
