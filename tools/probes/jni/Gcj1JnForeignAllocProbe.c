/*
 * SPDX-License-Identifier: Apache-2.0
 * Copyright 2024-2026 Craton Software Company
 *
 * Native half of tools/probes/Gcj1JnForeignAllocProbe.java (gcj j1/jn,
 * 2026-10-19; `docs/known-issues/gc/gcj-j1jn-foreign-attached-jni-allocation-never-collects-20261019.md`).
 *
 * A host thread (CreateThread / pthread_create) attaches with
 * AttachCurrentThread and allocates `count` byte arrays of `kib` KiB with
 * NewByteArray, while the Java side only sleeps. Mode 0 drops each array at
 * once (DeleteLocalRef); mode 1 keeps the last RING arrays as locals, stamps
 * each with its index and checks every kept array's stamp after every
 * allocation (a collection the allocation ran must not leave a local naming
 * the wrong bytes). The first NewByteArray that returns NULL (an
 * OutOfMemoryError pending) stops the loop and is recorded.
 *
 * Build (Windows, MSVC x64 developer prompt, Temurin 25.0.3 headers):
 *   cl /LD /O1 /I"%JAVA_HOME%\include" /I"%JAVA_HOME%\include\win32" Gcj1JnForeignAllocProbe.c /Fegcj1jnfa.dll
 * Build (Linux):
 *   gcc -O1 -shared -fPIC -pthread -I"$JDK/include" -I"$JDK/include/linux" \
 *       -o libgcj1jnfa.so tools/probes/jni/Gcj1JnForeignAllocProbe.c
 *
 * Nothing here blocks inside a native method: `start` returns at once, and
 * `join` is called only after `finished` said the attached thread detached.
 */
#include <jni.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#ifdef _WIN32
#include <windows.h>
typedef HANDLE pthread_t;
static void *(*win_thread_fn)(void *);
static DWORD WINAPI win_thread_main(LPVOID arg)
{
    win_thread_fn(arg);
    return 0;
}
static int pthread_create(pthread_t *t, void *attr, void *(*fn)(void *), void *arg)
{
    (void)attr;
    win_thread_fn = fn;
    *t = CreateThread(NULL, 0, win_thread_main, arg, 0, NULL);
    return *t == NULL;
}
static int pthread_join(pthread_t t, void **ret)
{
    (void)ret;
    WaitForSingleObject(t, INFINITE);
    CloseHandle(t);
    return 0;
}
#define ATOMIC_STORE_RELEASE(p, v) InterlockedExchange((volatile LONG *)(p), (v))
#define ATOMIC_LOAD_ACQUIRE(p) InterlockedCompareExchange((volatile LONG *)(p), 0, 0)
#else
#include <pthread.h>
#define ATOMIC_STORE_RELEASE(p, v) __atomic_store_n((p), (v), __ATOMIC_RELEASE)
#define ATOMIC_LOAD_ACQUIRE(p) __atomic_load_n((p), __ATOMIC_ACQUIRE)
#endif

#define RING 8

static JavaVM *g_vm;
static pthread_t g_thread;
static int g_count;
static int g_kib;
static int g_mode;
/* Results, written by the attached thread before `g_done`. */
static int g_attach_bad;
static int g_allocated;
static int g_oome_at = -1;
static int g_bad;
static volatile long g_done;

static int stamp_of(int i)
{
    return (i * 31 + 7) & 0x7f;
}

static void *foreign_main(void *arg)
{
    JNIEnv *env = NULL;
    jbyteArray ring[RING];
    int ring_index[RING];
    int i, k;
    jsize len = (jsize)g_kib * 1024;
    (void)arg;
    memset(ring, 0, sizeof ring);
    if ((*g_vm)->AttachCurrentThread(g_vm, (void **)&env, NULL) != JNI_OK || env == NULL) {
        g_attach_bad = 1;
        ATOMIC_STORE_RELEASE(&g_done, 1);
        return NULL;
    }
    for (i = 0; i < g_count; i++) {
        jbyteArray a = (*env)->NewByteArray(env, len);
        if (a == NULL || (*env)->ExceptionCheck(env)) {
            (*env)->ExceptionClear(env);
            g_oome_at = i;
            break;
        }
        g_allocated++;
        if (g_mode == 0) {
            (*env)->DeleteLocalRef(env, a);
            continue;
        }
        {
            jbyte s = (jbyte)stamp_of(i);
            (*env)->SetByteArrayRegion(env, a, 0, 1, &s);
            (*env)->SetByteArrayRegion(env, a, len - 1, 1, &s);
        }
        k = i % RING;
        if (ring[k] != NULL) {
            (*env)->DeleteLocalRef(env, ring[k]);
        }
        ring[k] = a;
        ring_index[k] = i;
        for (k = 0; k < RING; k++) {
            jbyte first = 0, last = 0;
            if (ring[k] == NULL) {
                continue;
            }
            (*env)->GetByteArrayRegion(env, ring[k], 0, 1, &first);
            (*env)->GetByteArrayRegion(env, ring[k], len - 1, 1, &last);
            if (first != (jbyte)stamp_of(ring_index[k]) || last != first
                || (*env)->GetArrayLength(env, ring[k]) != len) {
                g_bad++;
            }
        }
    }
    for (k = 0; k < RING; k++) {
        if (ring[k] != NULL) {
            (*env)->DeleteLocalRef(env, ring[k]);
        }
    }
    (*g_vm)->DetachCurrentThread(g_vm);
    ATOMIC_STORE_RELEASE(&g_done, 1);
    return NULL;
}

JNIEXPORT jint JNICALL Java_Gcj1JnForeignAllocProbe_start(JNIEnv *env, jclass cls, jint mode,
                                                         jint count, jint kib)
{
    (void)cls;
    if ((*env)->GetJavaVM(env, &g_vm) != JNI_OK) {
        return -1;
    }
    g_mode = mode;
    g_count = count;
    g_kib = kib;
    return pthread_create(&g_thread, NULL, foreign_main, NULL);
}

JNIEXPORT jboolean JNICALL Java_Gcj1JnForeignAllocProbe_finished(JNIEnv *env, jclass cls)
{
    (void)env;
    (void)cls;
    return ATOMIC_LOAD_ACQUIRE(&g_done) ? JNI_TRUE : JNI_FALSE;
}

/* out: attach_bad, allocated, oome_at, bad. */
JNIEXPORT void JNICALL Java_Gcj1JnForeignAllocProbe_join(JNIEnv *env, jclass cls, jintArray out)
{
    jint r[4];
    (void)cls;
    pthread_join(g_thread, NULL);
    r[0] = g_attach_bad;
    r[1] = g_allocated;
    r[2] = g_oome_at;
    r[3] = g_bad;
    (*env)->SetIntArrayRegion(env, out, 0, 4, r);
}
