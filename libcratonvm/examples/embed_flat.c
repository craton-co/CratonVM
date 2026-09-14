/* SPDX-License-Identifier: Apache-2.0
 * Copyright 2024-2026 Craton Software Company
 *
 * embed_flat.c — Layer 2 (flat opaque-handle C API) acceptance harness for
 * libcratonvm.
 *
 * Where embed_smoke.c drives the raw JNI Invocation API + the 234-slot JNIEnv
 * function table (the `libjvm`-substitute path), this harness exercises the
 * *curated* flat `cratonvm_*` C API: a host that just wants
 *
 *     create -> load_class -> new_string -> invoke_static -> last_error -> destroy
 *
 * without indexing JNIEnv slots by hand. No Rust types cross the boundary;
 * everything below is opaque handles + POD.
 *
 * It is NOT compiled by `cargo`; the orchestrator builds it against the
 * produced shared/static library. Build commands (run from the repo root after
 * `cargo build -p libcratonvm`). cargo names the artifacts after the crate
 * (`libcratonvm`): Windows `libcratonvm.dll` + `libcratonvm.dll.lib`, Linux
 * `liblibcratonvm.so` / `liblibcratonvm.a` (linked `-llibcratonvm`). Substitute
 * `release` for `debug` for a release build.
 *
 *   Linux (shared):
 *     cc embed_flat.c -L target/release -llibcratonvm -ldl -lpthread \
 *        -o embed_flat
 *     LD_LIBRARY_PATH=target/release ./embed_flat
 *
 *   Windows (MSVC, links the import lib for libcratonvm.dll):
 *     cl /Fe:embed_flat.exe embed_flat.c target\release\libcratonvm.dll.lib
 *     copy target\release\libcratonvm.dll .
 *     embed_flat.exe
 *
 *   Static link (any platform), against liblibcratonvm.a / libcratonvm.lib:
 *     cc embed_flat.c target/release/liblibcratonvm.a -ldl -lpthread \
 *        -o embed_flat
 *
 * The reproducible build+run wrapper is scripts/build-libcratonvm.ps1.
 *
 * This file re-declares the slice of the libcratonvm flat C ABI it touches so
 * it builds standalone. The declarations below match the `#[repr(C)]` types in
 * libcratonvm/src/lib.rs exactly and are kept ABI-compatible with the public
 * header at libcratonvm/include/cratonvm.h — a real host would instead do
 * `#include "cratonvm.h"` (add `-I libcratonvm/include` to the build command)
 * and delete the re-declarations below.
 */

#include <stdint.h>
#include <stdio.h>
#include <string.h>

/* ---- libcratonvm flat C ABI surface ----------------------------------- */

typedef int32_t jint;
typedef uint8_t jboolean;

/* Opaque VM handle. */
typedef struct CratonVm CratonVm;

/* u64 handles: object/string ref (0 == null) and class id. */
typedef uint64_t CratonRef;
typedef uint64_t CratonClass;

/* JavaVMInitArgs / JavaVMOption: same layout as jni.h, reused by the flat
 * `cratonvm_create`. Pass NULL to cratonvm_create for defaults. */
typedef struct JavaVMOption {
    char *optionString;
    void *extraInfo;
} JavaVMOption;

typedef struct JavaVMInitArgs {
    jint version;
    jint nOptions;
    JavaVMOption *options;
    jboolean ignoreUnrecognized;
} JavaVMInitArgs;

/* CratonValue tags — see `craton_tag` in libcratonvm/src/lib.rs. */
#define CRATON_TAG_VOID    0
#define CRATON_TAG_INT     1
#define CRATON_TAG_LONG    2
#define CRATON_TAG_FLOAT   3
#define CRATON_TAG_DOUBLE  4
#define CRATON_TAG_OBJECT  5
#define CRATON_TAG_ERROR  (-1)

/* C-ABI tagged value. payload interpretation depends on tag (see header doc):
 *   INT    -> (int32_t)payload
 *   LONG   -> (int64_t)payload
 *   FLOAT  -> bit-cast of low 32 bits to float
 *   DOUBLE -> bit-cast of payload to double
 *   OBJECT -> CratonRef
 */
typedef struct CratonValue {
    jint     tag;
    uint64_t payload;
} CratonValue;

#define JNI_OK   0
#define JNI_ERR (-1)
#define JNI_VERSION_1_8 0x00010008

extern CratonVm   *cratonvm_create(const JavaVMInitArgs *args);
extern void        cratonvm_destroy(CratonVm *vm);
extern jint        cratonvm_release_ref(CratonVm *vm, CratonRef reference);
extern jint        cratonvm_load_class(CratonVm *vm, const char *name, CratonClass *out_class);
extern CratonValue cratonvm_invoke_static(CratonVm *vm, const char *cls,
                       const char *method, const char *sig,
                       const CratonValue *args, jint n_args);
extern CratonRef   cratonvm_new_string(CratonVm *vm, const char *utf8);
extern const char *cratonvm_last_error(CratonVm *vm);
extern void        cratonvm_clear_error(CratonVm *vm);

static void print_err(CratonVm *vm, const char *where_) {
    const char *e = cratonvm_last_error(vm);
    fprintf(stderr, "%s: %s\n", where_, e ? e : "(no error message)");
}

int main(void) {
    /* 1. Create the VM with a tiny heap + a system property (defaults are also
     *    fine: pass NULL). */
    JavaVMOption opts[2];
    opts[0].optionString = (char *)"-Xmx64m";
    opts[0].extraInfo = NULL;
    opts[1].optionString = (char *)"-Dembed.flat=1";
    opts[1].extraInfo = NULL;

    JavaVMInitArgs args;
    memset(&args, 0, sizeof(args));
    args.version = JNI_VERSION_1_8;
    args.nOptions = 2;
    args.options = opts;
    args.ignoreUnrecognized = 1;

    CratonVm *vm = cratonvm_create(&args);
    if (vm == NULL) {
        print_err(NULL, "cratonvm_create");
        return 1;
    }
    printf("cratonvm_create ok: vm=%p\n", (void *)vm);

    /* 2. Load a bootstrap class. */
    CratonClass cls = 0;
    if (cratonvm_load_class(vm, "java/lang/System", &cls) != JNI_OK) {
        print_err(vm, "cratonvm_load_class(java/lang/System)");
        cratonvm_destroy(vm);
        return 1;
    }
    printf("cratonvm_load_class ok: class handle=%llu\n", (unsigned long long)cls);

    /* 3. Create a Java String handle (usable as an OBJECT arg). */
    CratonRef s = cratonvm_new_string(vm, "hello from C");
    if (s == 0) {
        print_err(vm, "cratonvm_new_string");
        cratonvm_destroy(vm);
        return 1;
    }
    printf("cratonvm_new_string ok: ref=0x%llx\n", (unsigned long long)s);

    /* 4. Invoke a void static method with no args: System.gc(). */
    CratonValue r = cratonvm_invoke_static(vm, "java/lang/System", "gc", "()V", NULL, 0);
    if (r.tag == CRATON_TAG_ERROR) {
        print_err(vm, "cratonvm_invoke_static(System.gc)");
        cratonvm_destroy(vm);
        return 1;
    }
    printf("cratonvm_invoke_static(System.gc) ok: tag=%d\n", r.tag);

    /* 5. Drive the last-error path on purpose: a bad class name must fail and
     *    leave a message. */
    CratonClass bad = 0;
    if (cratonvm_load_class(vm, "no/such/Class", &bad) == JNI_OK) {
        fprintf(stderr, "expected load_class(no/such/Class) to fail\n");
        cratonvm_destroy(vm);
        return 1;
    }
    {
        const char *e = cratonvm_last_error(vm);
        printf("expected error captured: %s\n", e ? e : "(none?!)");
        if (e == NULL) {
            fprintf(stderr, "last_error should have been set after a bad load\n");
            cratonvm_destroy(vm);
            return 1;
        }
    }

    /* 6. Release object refs, then tear down. */
    if (cratonvm_release_ref(vm, s) != JNI_OK) {
        print_err(vm, "cratonvm_release_ref");
        cratonvm_destroy(vm);
        return 1;
    }
    cratonvm_destroy(vm);
    printf("embed_flat: OK\n");
    return 0;
}
