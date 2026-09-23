/* SPDX-License-Identifier: Apache-2.0
 * Copyright 2024-2026 Craton Software Company
 *
 * cratonvm_helpers.h — OPTIONAL header-only convenience layer over the flat
 * libcratonvm C ABI (cratonvm.h).
 *
 * This is the "C-varargs convenience shim" noted in
 * docs/feature-designs/embedding-api.md. A *definition-side* C-varargs entry
 * (`extern "C" fn(...)`) is unsound / unavailable on stable Rust and not
 * ABI-portable for non-int/double argument types, so the stable core ABI takes
 * a typed `CratonValue` array + count. This header restores varargs-like
 * ERGONOMICS purely on the C side — no extra ABI surface, no Rust changes:
 *
 *   - `cratonvm_val_*` build a CratonValue from a native C value;
 *   - `cratonvm_as_*` read a CratonValue back into a native C value;
 *   - `cratonvm_invoke_static_v` / `cratonvm_invoke_virtual_v` are variadic
 *     macros that assemble the typed array + count for you via a C99 compound
 *     literal and `sizeof`.
 *
 * It is entirely OPTIONAL: a host can ignore it and call the core functions in
 * cratonvm.h directly. It is kept in a SEPARATE header (not cratonvm.h) so the
 * core header stays a faithful, cbindgen-regenerable mirror of the Rust ABI.
 *
 * Requires C99 for the varargs-like macros (compound literals, variadic
 * macros). The inline constructors/readers are C++11-compatible, but the
 * CRATONVM_ARGS / cratonvm_invoke_*_v macros are not standard C++ because they
 * rely on C99 compound literals. The float/double constructors/readers bit-cast
 * via memcpy to avoid aliasing UB.
 */

#ifndef CRATONVM_HELPERS_H
#define CRATONVM_HELPERS_H

#include "cratonvm.h"

#include <stdint.h>
#include <string.h>

#ifdef __cplusplus
extern "C" {
#endif

/* ---- CratonValue constructors ---------------------------------------- */

static inline CratonValue cratonvm_val_int(int32_t v) {
    CratonValue cv;
    cv.tag = CRATON_TAG_INT;
    cv.payload = (uint64_t)(uint32_t)v;
    return cv;
}

static inline CratonValue cratonvm_val_long(int64_t v) {
    CratonValue cv;
    cv.tag = CRATON_TAG_LONG;
    cv.payload = (uint64_t)v;
    return cv;
}

static inline CratonValue cratonvm_val_float(float v) {
    uint32_t bits;
    memcpy(&bits, &v, sizeof(bits));
    CratonValue cv;
    cv.tag = CRATON_TAG_FLOAT;
    cv.payload = (uint64_t)bits;
    return cv;
}

static inline CratonValue cratonvm_val_double(double v) {
    uint64_t bits;
    memcpy(&bits, &v, sizeof(bits));
    CratonValue cv;
    cv.tag = CRATON_TAG_DOUBLE;
    cv.payload = bits;
    return cv;
}

static inline CratonValue cratonvm_val_object(CratonRef r) {
    CratonValue cv;
    cv.tag = CRATON_TAG_OBJECT;
    cv.payload = (uint64_t)r;
    return cv;
}

static inline CratonValue cratonvm_val_void(void) {
    CratonValue cv;
    cv.tag = CRATON_TAG_VOID;
    cv.payload = 0;
    return cv;
}

/* ---- CratonValue readers --------------------------------------------- */

static inline int      cratonvm_is_error(CratonValue v) { return v.tag == CRATON_TAG_ERROR; }
static inline int32_t  cratonvm_as_int(CratonValue v)   { return (int32_t)(uint32_t)v.payload; }
static inline int64_t  cratonvm_as_long(CratonValue v)  { return (int64_t)v.payload; }
static inline CratonRef cratonvm_as_object(CratonValue v) { return (CratonRef)v.payload; }

static inline float cratonvm_as_float(CratonValue v) {
    uint32_t bits = (uint32_t)v.payload;
    float f;
    memcpy(&f, &bits, sizeof(f));
    return f;
}

static inline double cratonvm_as_double(CratonValue v) {
    uint64_t bits = v.payload;
    double d;
    memcpy(&d, &bits, sizeof(d));
    return d;
}

/* ---- varargs-like invoke macros -------------------------------------- */

/* Assemble a typed CratonValue array + its element count from a variadic list
 * of CratonValue arguments (each built with a cratonvm_val_* constructor).
 *
 * NOTE: these require AT LEAST ONE argument — a standard C99 compound literal
 * cannot be empty. For a zero-argument call, use the core function directly
 * with a NULL args pointer and 0 count:
 *     cratonvm_invoke_virtual(vm, recv, "length", "()I", NULL, 0);
 */
#define CRATONVM_ARGS(...)  ((const CratonValue[]){ __VA_ARGS__ })
#define CRATONVM_NARGS(...) \
    ((cratonvm_jint)(sizeof((const CratonValue[]){ __VA_ARGS__ }) / sizeof(CratonValue)))

/* cratonvm_invoke_static_v(vm, cls, method, sig, arg0, arg1, ...) */
#define cratonvm_invoke_static_v(vm, cls, method, sig, ...)              \
    cratonvm_invoke_static((vm), (cls), (method), (sig),                 \
                           CRATONVM_ARGS(__VA_ARGS__),                   \
                           CRATONVM_NARGS(__VA_ARGS__))

/* cratonvm_invoke_virtual_v(vm, receiver, method, sig, arg0, arg1, ...) */
#define cratonvm_invoke_virtual_v(vm, receiver, method, sig, ...)        \
    cratonvm_invoke_virtual((vm), (receiver), (method), (sig),           \
                            CRATONVM_ARGS(__VA_ARGS__),                  \
                            CRATONVM_NARGS(__VA_ARGS__))

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* CRATONVM_HELPERS_H */
