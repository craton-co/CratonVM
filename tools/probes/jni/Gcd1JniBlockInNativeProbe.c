/*
 * SPDX-License-Identifier: Apache-2.0
 * Copyright 2024-2026 Craton Software Company
 *
 * Native half of tools/bench/Gcd1JniBlockInNativeProbe.java (gcd d3/k,
 * 2026-09-27). Linux. Build and run instructions are in the Java class
 * comment.
 *
 * `blockInC` waits in C -- no JNI call in its loop -- for a flag another
 * thread sets through `release`, as a native blocked in `pthread_join`,
 * `read` or `poll` would. `pollInNative` makes JNI calls while it waits.
 * Both give up after `max_ms`, so the probe ends on a VM that holds its
 * collections for them too (the defect:
 * `docs/known-issues/gc/gcd-d2i-jni-native-methods-are-counted-mutators-20260927.md`).
 * A signal may cut a `usleep` short; every loop re-checks the clock.
 */
#include <jni.h>
#include <time.h>
#include <unistd.h>

static int g_entered;
static int g_release;

static long long now_ms(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (long long)ts.tv_sec * 1000LL + ts.tv_nsec / 1000000L;
}

JNIEXPORT void JNICALL
Java_Gcd1JniBlockInNativeProbe_reset(JNIEnv *env, jclass self)
{
    (void)env;
    (void)self;
    __atomic_store_n(&g_entered, 0, __ATOMIC_SEQ_CST);
    __atomic_store_n(&g_release, 0, __ATOMIC_SEQ_CST);
}

JNIEXPORT jboolean JNICALL
Java_Gcd1JniBlockInNativeProbe_entered(JNIEnv *env, jclass self)
{
    (void)env;
    (void)self;
    return __atomic_load_n(&g_entered, __ATOMIC_SEQ_CST) ? JNI_TRUE : JNI_FALSE;
}

JNIEXPORT void JNICALL
Java_Gcd1JniBlockInNativeProbe_release(JNIEnv *env, jclass self)
{
    (void)env;
    (void)self;
    __atomic_store_n(&g_release, 1, __ATOMIC_SEQ_CST);
}

/*
 * `o` is a local ref. Take a global ref to the same object, wait in C, then
 * ask whether the native's own copy of `o` still names that object and still
 * reads the field. Bit 0 = timed out; bit 1 = IsSameObject(o, g) false;
 * bit 2 = the int field read back wrong.
 */
JNIEXPORT jint JNICALL
Java_Gcd1JniBlockInNativeProbe_blockInC(JNIEnv *env, jclass self, jobject o, jint expected,
                                        jint max_ms)
{
    jint code = 0;
    jclass hc = (*env)->GetObjectClass(env, o);
    jfieldID fv = (*env)->GetFieldID(env, hc, "v", "I");
    jobject g = (*env)->NewGlobalRef(env, o);
    long long deadline = now_ms() + max_ms;
    (void)self;
    __atomic_store_n(&g_entered, 1, __ATOMIC_SEQ_CST);
    while (!__atomic_load_n(&g_release, __ATOMIC_SEQ_CST)) {
        if (now_ms() >= deadline) {
            code |= 1;
            break;
        }
        usleep(1000);
    }
    if (!(*env)->IsSameObject(env, o, g)) {
        code |= 2;
    } else if ((*env)->GetIntField(env, o, fv) != expected) {
        code |= 4;
    }
    (*env)->DeleteGlobalRef(env, g);
    return code;
}

/*
 * Poll the class's `static volatile boolean released` through JNI, with a
 * string allocated, measured and deleted per round. Bit 0 = timed out; bit 1 =
 * a string came back NULL or with the wrong length.
 */
JNIEXPORT jint JNICALL
Java_Gcd1JniBlockInNativeProbe_pollInNative(JNIEnv *env, jclass self, jint max_ms)
{
    jint code = 0;
    jfieldID fr = (*env)->GetStaticFieldID(env, self, "released", "Z");
    long long deadline = now_ms() + max_ms;
    if (fr == NULL) {
        (*env)->ExceptionClear(env);
        return 3;
    }
    __atomic_store_n(&g_entered, 1, __ATOMIC_SEQ_CST);
    for (;;) {
        jstring s;
        if ((*env)->GetStaticBooleanField(env, self, fr)) {
            break;
        }
        if (now_ms() >= deadline) {
            code |= 1;
            break;
        }
        s = (*env)->NewStringUTF(env, "poll");
        if (s == NULL || (*env)->GetStringUTFLength(env, s) != 4) {
            code |= 2;
        }
        if (s != NULL) {
            (*env)->DeleteLocalRef(env, s);
        }
        usleep(500);
    }
    return code;
}
