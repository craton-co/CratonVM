/*
 * SPDX-License-Identifier: Apache-2.0
 * Copyright 2024-2026 Craton Software Company
 *
 * Native half of tools/probes/GciJc13JniCriticalWeakProbe.java (gci i13/jc,
 * 2026-10-13): critical array sections and their release modes, the string
 * copies (exact units, NUL terminator), NewDirectByteBuffer, GetObjectRefType,
 * PopLocalFrame of a global, EnsureLocalCapacity / PushLocalFrame return
 * codes, and a weak global polled with IsSameObject(w, NULL).
 *
 * Windows: cl /LD /O1 /I%JAVA_HOME%\include /I%JAVA_HOME%\include\win32
 *          GciJc13JniCriticalWeakProbe.c /Fegcijc13.dll
 * Linux:   gcc -O1 -shared -fPIC -I$JAVA_HOME/include -I$JAVA_HOME/include/linux
 *          GciJc13JniCriticalWeakProbe.c -o libgcijc13.so
 */
#include <jni.h>
#include <stddef.h>

/* Critical section: every element times ten, released with `mode`. Returns
 * the isCopy the VM reported (0 / 1), or -1 when the Get failed. */
JNIEXPORT jint JNICALL
Java_GciJc13JniCriticalWeakProbe_criticalScale(JNIEnv *env, jclass self, jintArray a, jint mode)
{
    jboolean is_copy = 2;
    jsize n;
    jsize i;
    jint *p;
    (void)self;
    n = (*env)->GetArrayLength(env, a);
    p = (jint *)(*env)->GetPrimitiveArrayCritical(env, a, &is_copy);
    if (p == NULL) {
        return -1;
    }
    for (i = 0; i < n; i++) {
        p[i] = p[i] * 10;
    }
    (*env)->ReleasePrimitiveArrayCritical(env, a, p, mode);
    return (jint)is_copy;
}

/* NewString(GetStringChars(s), len). */
JNIEXPORT jstring JNICALL
Java_GciJc13JniCriticalWeakProbe_charsRoundTrip(JNIEnv *env, jclass self, jstring s)
{
    const jchar *c;
    jsize n;
    jstring back;
    (void)self;
    n = (*env)->GetStringLength(env, s);
    c = (*env)->GetStringChars(env, s, NULL);
    if (c == NULL) {
        return NULL;
    }
    back = (*env)->NewString(env, c, n);
    (*env)->ReleaseStringChars(env, s, c);
    return back;
}

/* 1 when GetStringChars' buffer has a NUL unit at index len (HotSpot
 * allocates len + 1 units and zero-terminates), else 0; -1 on failure. */
JNIEXPORT jint JNICALL
Java_GciJc13JniCriticalWeakProbe_charsTerminated(JNIEnv *env, jclass self, jstring s)
{
    const jchar *c;
    jsize n;
    jint r;
    (void)self;
    n = (*env)->GetStringLength(env, s);
    c = (*env)->GetStringChars(env, s, NULL);
    if (c == NULL) {
        return -1;
    }
    r = c[n] == 0 ? 1 : 0;
    (*env)->ReleaseStringChars(env, s, c);
    return r;
}

/* GetStringCritical, copied out inside the section, then NewString. */
JNIEXPORT jstring JNICALL
Java_GciJc13JniCriticalWeakProbe_criticalRoundTrip(JNIEnv *env, jclass self, jstring s)
{
    jchar buf[64];
    const jchar *c;
    jsize n;
    jsize i;
    (void)self;
    n = (*env)->GetStringLength(env, s);
    if (n > 64) {
        return NULL;
    }
    c = (*env)->GetStringCritical(env, s, NULL);
    if (c == NULL) {
        return NULL;
    }
    for (i = 0; i < n; i++) {
        buf[i] = c[i];
    }
    (*env)->ReleaseStringCritical(env, s, c);
    return (*env)->NewString(env, buf, n);
}

/* GetStringRegion of the whole string, then NewString. */
JNIEXPORT jstring JNICALL
Java_GciJc13JniCriticalWeakProbe_regionRoundTrip(JNIEnv *env, jclass self, jstring s)
{
    jchar buf[64];
    jsize n;
    (void)self;
    n = (*env)->GetStringLength(env, s);
    if (n > 64) {
        return NULL;
    }
    (*env)->GetStringRegion(env, s, 0, n, buf);
    return (*env)->NewString(env, buf, n);
}

static unsigned char g_backing[64] = {0, 0, 0, 1};

JNIEXPORT jobject JNICALL
Java_GciJc13JniCriticalWeakProbe_newDirect(JNIEnv *env, jclass self, jlong cap)
{
    (void)self;
    return (*env)->NewDirectByteBuffer(env, g_backing, cap);
}

JNIEXPORT jobject JNICALL
Java_GciJc13JniCriticalWeakProbe_newDirectNull(JNIEnv *env, jclass self)
{
    (void)self;
    return (*env)->NewDirectByteBuffer(env, NULL, 0);
}

JNIEXPORT jint JNICALL
Java_GciJc13JniCriticalWeakProbe_directAddressIsNull(JNIEnv *env, jclass self, jobject b)
{
    (void)self;
    return (*env)->GetDirectBufferAddress(env, b) == NULL ? 1 : 0;
}

/* (address == NULL) << 32 | capacity, for any java.nio.Buffer. */
JNIEXPORT jlong JNICALL
Java_GciJc13JniCriticalWeakProbe_directViewProbe(JNIEnv *env, jclass self, jobject b)
{
    jlong is_null;
    jlong cap;
    (void)self;
    is_null = (*env)->GetDirectBufferAddress(env, b) == NULL ? 1 : 0;
    cap = (*env)->GetDirectBufferCapacity(env, b);
    return (is_null << 32) | (cap & 0xffffffffLL);
}

/* GetStringCritical's isCopy (0 / 1), or -1 when the Get failed. */
JNIEXPORT jint JNICALL
Java_GciJc13JniCriticalWeakProbe_stringCriticalIsCopy(JNIEnv *env, jclass self, jstring s)
{
    jboolean is_copy = 2;
    const jchar *c;
    (void)self;
    c = (*env)->GetStringCritical(env, s, &is_copy);
    if (c == NULL) {
        return -1;
    }
    (*env)->ReleaseStringCritical(env, s, c);
    return (jint)is_copy;
}

/* out = {local, global, weak, deleted global, deleted weak, pop(global),
 *        IsSameObject(pop result, o) after DeleteGlobalRef of the global}. */
JNIEXPORT void JNICALL
Java_GciJc13JniCriticalWeakProbe_refTypes(JNIEnv *env, jclass self, jobject o, jintArray out)
{
    jint v[7];
    jobject g;
    jweak w;
    jobject r;
    (void)self;
    v[0] = (jint)(*env)->GetObjectRefType(env, o);
    g = (*env)->NewGlobalRef(env, o);
    v[1] = (jint)(*env)->GetObjectRefType(env, g);
    w = (*env)->NewWeakGlobalRef(env, o);
    v[2] = (jint)(*env)->GetObjectRefType(env, w);
    (*env)->DeleteGlobalRef(env, g);
    v[3] = (jint)(*env)->GetObjectRefType(env, g);
    (*env)->DeleteWeakGlobalRef(env, w);
    v[4] = (jint)(*env)->GetObjectRefType(env, w);
    g = (*env)->NewGlobalRef(env, o);
    if ((*env)->PushLocalFrame(env, 4) != JNI_OK) {
        v[5] = -1;
        v[6] = -1;
    } else {
        r = (*env)->PopLocalFrame(env, g);
        v[5] = (jint)(*env)->GetObjectRefType(env, r);
        (*env)->DeleteGlobalRef(env, g);
        v[6] = (*env)->IsSameObject(env, r, o) ? 1 : 0;
    }
    (*env)->SetIntArrayRegion(env, out, 0, 7, v);
}

/* out = {return code, exception pending}. The exception, if any, is cleared;
 * a successful PushLocalFrame is popped. */
JNIEXPORT void JNICALL
Java_GciJc13JniCriticalWeakProbe_capacity(JNIEnv *env, jclass self, jint cap, jboolean ensure,
                                          jintArray out)
{
    jint v[2];
    (void)self;
    if (ensure) {
        v[0] = (*env)->EnsureLocalCapacity(env, cap);
    } else {
        v[0] = (*env)->PushLocalFrame(env, cap);
    }
    v[1] = (*env)->ExceptionCheck(env) ? 1 : 0;
    if (v[1]) {
        (*env)->ExceptionClear(env);
    }
    if (!ensure && v[0] == JNI_OK) {
        (*env)->PopLocalFrame(env, NULL);
    }
    (*env)->SetIntArrayRegion(env, out, 0, 2, v);
}

static jweak g_weak;

JNIEXPORT void JNICALL
Java_GciJc13JniCriticalWeakProbe_makeWeak(JNIEnv *env, jclass self, jobject o)
{
    (void)self;
    g_weak = (*env)->NewWeakGlobalRef(env, o);
}

JNIEXPORT jboolean JNICALL
Java_GciJc13JniCriticalWeakProbe_weakIsSame(JNIEnv *env, jclass self, jobject o)
{
    (void)self;
    return (*env)->IsSameObject(env, g_weak, o);
}

/* The "was it collected?" idiom: IsSameObject(w, NULL), nothing else. */
JNIEXPORT jboolean JNICALL
Java_GciJc13JniCriticalWeakProbe_weakCleared(JNIEnv *env, jclass self)
{
    (void)self;
    return (*env)->IsSameObject(env, g_weak, NULL);
}

JNIEXPORT void JNICALL
Java_GciJc13JniCriticalWeakProbe_deleteWeak(JNIEnv *env, jclass self)
{
    (void)self;
    if (g_weak != NULL) {
        (*env)->DeleteWeakGlobalRef(env, g_weak);
        g_weak = NULL;
    }
}
