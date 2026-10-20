/*
 * SPDX-License-Identifier: Apache-2.0
 * Copyright 2024-2026 Craton Software Company
 *
 * Native half of tools/probes/GciPn14CriticalPinStormProbe.java (gci i14/pn,
 * 2026-10-13): JNI critical sections held open (an OS sleep inside the
 * section) while a Java thread churns the heap and calls System.gc(). No JNI
 * call is made inside a section: lengths are read before the Get, results are
 * stored after the Release.
 *
 * Windows: cl /LD /O1 /I%JAVA_HOME%\include /I%JAVA_HOME%\include\win32
 *          GciPn14CriticalPinStormProbe.c /Fegcipn14.dll
 * Linux:   gcc -O1 -shared -fPIC -I$JAVA_HOME/include -I$JAVA_HOME/include/linux
 *          GciPn14CriticalPinStormProbe.c -o libgcipn14.so
 */
#include <jni.h>
#include <stddef.h>
#ifdef _WIN32
#include <windows.h>
static void sleep_ms(int ms)
{
    Sleep((DWORD)ms);
}
#else
#include <time.h>
static void sleep_ms(int ms)
{
    struct timespec ts;
    ts.tv_sec = ms / 1000;
    ts.tv_nsec = (long)(ms % 1000) * 1000000L;
    nanosleep(&ts, NULL);
}
#endif

static volatile int g_inside = 0;

JNIEXPORT jint JNICALL
Java_GciPn14CriticalPinStormProbe_inside(JNIEnv *env, jclass self)
{
    (void)env;
    (void)self;
    return g_inside;
}

/* Write seed + i through the section's pointer, sleep with the section open,
 * count the elements that changed under the pointer, write seed + i + 1,
 * release with `mode`. Returns the count (-1: the Get failed). */
JNIEXPORT jint JNICALL
Java_GciPn14CriticalPinStormProbe_holdCritical(JNIEnv *env, jclass self, jintArray a, jint seed,
                                               jint ms, jint mode, jintArray isCopyOut)
{
    jboolean is_copy = 2;
    jsize n;
    jsize i;
    jint *p;
    jint bad = 0;
    jint ic;
    (void)self;
    n = (*env)->GetArrayLength(env, a);
    p = (jint *)(*env)->GetPrimitiveArrayCritical(env, a, &is_copy);
    if (p == NULL) {
        return -1;
    }
    for (i = 0; i < n; i++) {
        p[i] = seed + i;
    }
    g_inside = 1;
    sleep_ms(ms);
    for (i = 0; i < n; i++) {
        if (p[i] != seed + i) {
            bad++;
        }
        p[i] = seed + i + 1;
    }
    g_inside = 0;
    (*env)->ReleasePrimitiveArrayCritical(env, a, p, mode);
    ic = (jint)is_copy;
    (*env)->SetIntArrayRegion(env, isCopyOut, 0, 1, &ic);
    return bad;
}

/* Two sections open at once (the compression idiom): dst[i] = 2 * src[i],
 * sleep, re-check, release dst with 0 and src with JNI_ABORT. */
JNIEXPORT jint JNICALL
Java_GciPn14CriticalPinStormProbe_holdTwo(JNIEnv *env, jclass self, jintArray src, jintArray dst,
                                          jint ms)
{
    jsize n;
    jsize m;
    jsize i;
    jint *s;
    jint *d;
    jint bad = 0;
    (void)self;
    n = (*env)->GetArrayLength(env, src);
    m = (*env)->GetArrayLength(env, dst);
    if (m < n) {
        n = m;
    }
    s = (jint *)(*env)->GetPrimitiveArrayCritical(env, src, NULL);
    if (s == NULL) {
        return -1;
    }
    d = (jint *)(*env)->GetPrimitiveArrayCritical(env, dst, NULL);
    if (d == NULL) {
        (*env)->ReleasePrimitiveArrayCritical(env, src, s, JNI_ABORT);
        return -1;
    }
    for (i = 0; i < n; i++) {
        d[i] = s[i] * 2;
    }
    g_inside = 1;
    sleep_ms(ms);
    for (i = 0; i < n; i++) {
        if (d[i] != s[i] * 2) {
            bad++;
        }
    }
    g_inside = 0;
    (*env)->ReleasePrimitiveArrayCritical(env, dst, d, 0);
    (*env)->ReleasePrimitiveArrayCritical(env, src, s, JNI_ABORT);
    return bad;
}

/* GetStringCritical held across the sleep: the units read at Get are read
 * again through the same pointer. Returns the mismatch count (-1: the Get
 * failed, -2: the string is longer than the buffer). */
JNIEXPORT jint JNICALL
Java_GciPn14CriticalPinStormProbe_holdString(JNIEnv *env, jclass self, jstring str, jint ms,
                                             jintArray isCopyOut)
{
    jchar buf[512];
    const jchar *c;
    jboolean is_copy = 2;
    jsize n;
    jsize i;
    jint bad = 0;
    jint ic;
    (void)self;
    n = (*env)->GetStringLength(env, str);
    if (n > 512) {
        return -2;
    }
    c = (*env)->GetStringCritical(env, str, &is_copy);
    if (c == NULL) {
        return -1;
    }
    for (i = 0; i < n; i++) {
        buf[i] = c[i];
    }
    g_inside = 1;
    sleep_ms(ms);
    for (i = 0; i < n; i++) {
        if (c[i] != buf[i]) {
            bad++;
        }
    }
    g_inside = 0;
    (*env)->ReleaseStringCritical(env, str, c);
    ic = (jint)is_copy;
    (*env)->SetIntArrayRegion(env, isCopyOut, 0, 1, &ic);
    return bad;
}
