/*
 * SPDX-License-Identifier: Apache-2.0
 * Copyright 2024-2026 Craton Software Company
 *
 * Native half of tools/probes/Zr2R2JniRawLocal.java (zr z2/r2, 2026-10-12).
 * Every JNI call uses the `A` (jvalue array) form: no C varargs.
 *
 * Linux: gcc -O1 -Wall -shared -fPIC -I$J/include -I$J/include/linux \
 *          -o jnilib/libzr2r2jniraw.so tools/probes/jni/Zr2R2JniRawLocal.c
 * Windows: cl /LD /O1 /I"%JDK%\include" /I"%JDK%\include\win32" \
 *          Zr2R2JniRawLocal.c /Fezr2r2jniraw.dll
 */
#include <jni.h>
#ifdef _WIN32
#include <windows.h>
static unsigned long long now_ms(void) { return GetTickCount64(); }
#else
#include <time.h>
static unsigned long long now_ms(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (unsigned long long)ts.tv_sec * 1000ull + (unsigned long long)ts.tv_nsec / 1000000ull;
}
#endif

/*
 * Hold `h` in a C local, spin `ms` milliseconds with no JNI call at all, then
 * read `h.v` through the same local. 0 = the value is right.
 */
JNIEXPORT jint JNICALL
Java_Zr2R2JniRawLocal_spinThenRead(JNIEnv *env, jclass self, jobject h, jint expected, jint ms)
{
    jclass hc = (*env)->GetObjectClass(env, h);
    jfieldID fv = (*env)->GetFieldID(env, hc, "v", "I");
    volatile unsigned long long spins = 0;
    unsigned long long until = now_ms() + (unsigned long long)ms;
    (void)self;
    while (now_ms() < until) {
        spins++;
    }
    return (*env)->GetIntField(env, h, fv) == expected ? 0 : 1;
}

/*
 * Hold `h` in a C local across `churn.run()` (allocates, System.gc()), then
 * ask whether the local still names the object and still reads the field.
 */
JNIEXPORT jint JNICALL
Java_Zr2R2JniRawLocal_upcallThenRead(JNIEnv *env, jclass self, jobject h, jobject churn,
                                    jint expected)
{
    jint code = 0;
    jobject g = (*env)->NewGlobalRef(env, h);
    jclass rc = (*env)->GetObjectClass(env, churn);
    jmethodID run = (*env)->GetMethodID(env, rc, "run", "()V");
    jclass hc = (*env)->GetObjectClass(env, h);
    jfieldID fv = (*env)->GetFieldID(env, hc, "v", "I");
    (void)self;
    (*env)->CallVoidMethodA(env, churn, run, NULL);
    if ((*env)->ExceptionCheck(env)) {
        (*env)->ExceptionClear(env);
        code |= 4;
    }
    if (!(*env)->IsSameObject(env, h, g)) {
        code |= 1;
    } else if ((*env)->GetIntField(env, h, fv) != expected) {
        code |= 2;
    }
    (*env)->DeleteGlobalRef(env, g);
    return code;
}
