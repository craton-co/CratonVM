/* SPDX-License-Identifier: Apache-2.0
 * Copyright 2024-2026 Craton Software Company
 *
 * embed_smoke.c — Layer 1 (JNI Invocation API) acceptance harness for
 * libcratonvm.
 *
 * This is the canonical "host loads a libjvm substitute" flow: create a VM via
 * the JNI Invocation API, then drive a static method through the per-JNIEnv
 * function table — the exact path a C/C++/cgo/ctypes embedder uses.
 *
 * It is NOT compiled by `cargo`; the orchestrator builds it against the
 * produced shared/static library. Build commands (run from the repo root after
 * `cargo build -p libcratonvm`):
 *
 * NOTE on artifact names: cargo names the cdylib/staticlib after the crate
 * (`libcratonvm`), so on Windows the import lib is `libcratonvm.dll.lib` and the
 * runtime DLL is `libcratonvm.dll`; on Linux (crate name already starts with
 * `lib`) the files are `liblibcratonvm.so` / `liblibcratonvm.a`, linked with
 * `-llibcratonvm`. Substitute `release` for `debug` for a release build.
 *
 *   Linux (shared):
 *     cc embed_smoke.c -L target/release -llibcratonvm -ldl -lpthread \
 *        -o embed_smoke
 *     LD_LIBRARY_PATH=target/release ./embed_smoke
 *
 *   Windows (MSVC, links the import lib for libcratonvm.dll):
 *     cl /Fe:embed_smoke.exe embed_smoke.c target\release\libcratonvm.dll.lib
 *     copy target\release\libcratonvm.dll .
 *     embed_smoke.exe
 *
 *   Static link (any platform), against liblibcratonvm.a / libcratonvm.lib:
 *     cc embed_smoke.c target/release/liblibcratonvm.a -ldl -lpthread \
 *        -o embed_smoke
 *
 * The reproducible build+run wrapper is scripts/build-libcratonvm.ps1.
 *
 * This file deliberately re-declares the small slice of the JNI ABI it touches
 * so it builds without a JDK's <jni.h> present. The struct layouts below match
 * jni.h exactly and the libcratonvm public C ABI.
 */

#include <stdint.h>
#include <stdio.h>
#include <string.h>

/* ---- Minimal jni.h ABI surface ---------------------------------------- */

typedef int32_t jint;
typedef int32_t jsize;
typedef uint8_t jboolean;

/* JavaVM / JNIEnv are pointers to a pointer-to-function-table, per the JNI
 * spec (the Invocation-API double indirection): env -> *env (the table) ->
 * (*env)[i] (one slot). The VM hands back `*const *const usize` (a pointer to
 * the pointer to a `[usize; N]` table); we mirror that here.
 *
 * A table slot must be a *sized* type for `(*env)[index]` to be valid pointer
 * arithmetic, so we model it as `void *` (a function address, pointer-sized
 * like the Rust `usize`). Modelling it as bare `void` — as an earlier draft did
 * — compiles under GCC/Clang (which allow void-pointer arithmetic as an
 * extension) but is rejected by MSVC ("unknown size"); `void *` builds on all
 * three toolchains. */
typedef void     *JniSlot;    /* one function-table slot (a function address) */
typedef JniSlot  *JniTable;   /* the function table (array of slots) */
typedef JniTable *JNIEnv;     /* env -> table -> slot  (double indirection) */
typedef JniTable *JavaVM;     /* same shape; indexed for DestroyJavaVM (slot 3) */

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

#define JNI_OK 0
#define JNI_VERSION_1_8 0x00010008

/* The three exported Invocation-API entry points implemented by libcratonvm. */
extern jint JNI_GetDefaultJavaVMInitArgs(void *args);
extern jint JNI_CreateJavaVM(JavaVM *pvm, void **penv, void *args);
extern jint JNI_GetCreatedJavaVMs(JavaVM *vmBuf, jsize bufLen, jsize *nVMs);

/* ---- JNIEnv function-table slot indices we use -----------------------
 * These match the slot layout built in cratonvm-vm's native::jni
 * `build_function_table` (the standard JNI spec ordering). We use the
 * `...A` (jvalue-array) call form (slot 143) rather than the bare varargs
 * form (slot 141) because the VM populates the `_a` / `_v` variants; for a
 * zero-arg method the args pointer is simply NULL. */
#define JNIENV_FindClass             6   /* jclass    (*)(JNIEnv*, const char*) */
#define JNIENV_GetStaticMethodID     113 /* jmethodID (*)(JNIEnv*, jclass, const char*, const char*) */
#define JNIENV_CallStaticVoidMethodA 143 /* void      (*)(JNIEnv*, jclass, jmethodID, const jvalue*) */

/* JavaVM invocation-table slot indices (cratonvm-vm native::jni
 * `build_invoke_table`; standard JNI Invocation-API ordering). */
#define JAVAVM_DestroyJavaVM         3   /* jint (*)(JavaVM) */

typedef uint64_t jclass;
typedef uint64_t jmethodID;
typedef union jvalue { int32_t i; int64_t j; double d; uint64_t l; } jvalue;

typedef jclass    (*FindClass_t)(JNIEnv, const char *);
typedef jmethodID (*GetStaticMethodID_t)(JNIEnv, jclass, const char *, const char *);
typedef void      (*CallStaticVoidMethodA_t)(JNIEnv, jclass, jmethodID, const jvalue *);
typedef jint      (*DestroyJavaVM_t)(JavaVM);

static void *slot(JNIEnv env, int index) {
    /* env points at the table pointer; (*env) is the table; (*env)[index] is
     * the function pointer for that slot. */
    return (void *)((*env)[index]);
}

static void *vm_slot(JavaVM jvm, int index) {
    /* Same double-indirection shape as the JNIEnv table, applied to the
     * JavaVM invocation table. */
    return (void *)((*jvm)[index]);
}

int main(void) {
    /* 1. Discover the default init args / supported version. */
    JavaVMInitArgs vm_args;
    memset(&vm_args, 0, sizeof(vm_args));
    if (JNI_GetDefaultJavaVMInitArgs(&vm_args) != JNI_OK) {
        fprintf(stderr, "JNI_GetDefaultJavaVMInitArgs failed\n");
        return 1;
    }
    if (vm_args.version != JNI_VERSION_1_8) {
        fprintf(stderr, "unexpected version 0x%08x\n", vm_args.version);
        return 1;
    }
    printf("default init args: version=0x%08x\n", vm_args.version);

    /* 2. Build init args: a tiny heap + a classpath, mirroring a real host. */
    JavaVMOption opts[2];
    opts[0].optionString = (char *)"-Xmx64m";
    opts[0].extraInfo = NULL;
    opts[1].optionString = (char *)"-Dembed.smoke=1";
    opts[1].extraInfo = NULL;
    vm_args.version = JNI_VERSION_1_8;
    vm_args.nOptions = 2;
    vm_args.options = opts;
    vm_args.ignoreUnrecognized = 1;

    /* 3. Create the VM. */
    JavaVM jvm = NULL;
    JNIEnv env = NULL;
    jint rc = JNI_CreateJavaVM(&jvm, (void **)&env, &vm_args);
    if (rc != JNI_OK) {
        fprintf(stderr, "JNI_CreateJavaVM failed: %d\n", rc);
        return 1;
    }
    printf("JNI_CreateJavaVM ok: jvm=%p env=%p\n", (void *)jvm, (void *)env);

    /* 4. The VM must now be reported by JNI_GetCreatedJavaVMs. */
    JavaVM buf[1];
    jsize n = 0;
    if (JNI_GetCreatedJavaVMs(buf, 1, &n) != JNI_OK || n != 1) {
        fprintf(stderr, "JNI_GetCreatedJavaVMs: expected 1 VM, got %d\n", n);
        return 1;
    }
    printf("JNI_GetCreatedJavaVMs: %d VM(s)\n", n);

    /* 5. Drive a static method through the JNIEnv function table:
     *      System.gc()   ->  static void, no args, always present.
     * This exercises FindClass -> GetStaticMethodID -> CallStaticVoidMethod,
     * the same table native methods use. */
    FindClass_t             FindClass             = (FindClass_t)             slot(env, JNIENV_FindClass);
    GetStaticMethodID_t     GetStaticMethodID     = (GetStaticMethodID_t)     slot(env, JNIENV_GetStaticMethodID);
    CallStaticVoidMethodA_t CallStaticVoidMethodA = (CallStaticVoidMethodA_t) slot(env, JNIENV_CallStaticVoidMethodA);

    jclass cls = FindClass(env, "java/lang/System");
    if (cls == 0) {
        fprintf(stderr, "FindClass(java/lang/System) returned null\n");
        return 1;
    }
    jmethodID mid = GetStaticMethodID(env, cls, "gc", "()V");
    if (mid == 0) {
        fprintf(stderr, "GetStaticMethodID(System.gc) returned null\n");
        return 1;
    }
    CallStaticVoidMethodA(env, cls, mid, NULL);  /* zero-arg: args pointer is NULL */
    printf("invoked static System.gc() via JNIEnv table\n");

    /* 6. Tear the VM down via DestroyJavaVM (invocation-table slot 3). After a
     * successful destroy the VM is gone: JNI_GetCreatedJavaVMs must report 0. */
    DestroyJavaVM_t DestroyJavaVM = (DestroyJavaVM_t) vm_slot(jvm, JAVAVM_DestroyJavaVM);
    jint drc = DestroyJavaVM(jvm);
    if (drc != JNI_OK) {
        fprintf(stderr, "DestroyJavaVM failed: %d\n", drc);
        return 1;
    }
    n = -1;
    if (JNI_GetCreatedJavaVMs(buf, 1, &n) != JNI_OK || n != 0) {
        fprintf(stderr, "after DestroyJavaVM: expected 0 VMs, got %d\n", n);
        return 1;
    }
    printf("DestroyJavaVM ok: JNI_GetCreatedJavaVMs now reports %d VM(s)\n", n);

    printf("embed_smoke: OK\n");
    return 0;
}
