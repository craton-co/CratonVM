/*
 * SPDX-License-Identifier: Apache-2.0
 * Copyright 2024-2026 Craton Software Company
 *
 * Native half of tools/bench/GcfF2jJniRegionProbe.java (gcf f2/j,
 * 2026-10-01): what the JNI region functions write and raise at their edges
 * -- GetStringUTFRegion's terminator, and the exceptions of an out-of-range
 * or negative Get<Type>ArrayRegion / GetStringRegion. Build and run
 * instructions are in the Java class comment.
 */
#include <jni.h>
#include <string.h>

/* GetStringUTFRegion(s, start, len) into a buffer pre-filled with 'X':
 * answers the byte right after the region (0 = terminated, 'X' = not). */
JNIEXPORT jint JNICALL
Java_GcfF2jJniRegionProbe_utfRegionTail(JNIEnv *env, jclass self, jstring s, jint start, jint len)
{
    char buf[64];
    (void)self;
    memset(buf, 'X', sizeof buf);
    (*env)->GetStringUTFRegion(env, s, start, len, buf);
    if ((*env)->ExceptionCheck(env)) {
        return -1;
    }
    /* `len` UTF-16 units of ASCII text are `len` bytes. */
    return (jint)(unsigned char)buf[len];
}

/* GetIntArrayRegion(a, start, len) into a scratch buffer; any exception is
 * left pending for Java to catch and print. */
JNIEXPORT void JNICALL
Java_GcfF2jJniRegionProbe_intRegion(JNIEnv *env, jclass self, jintArray a, jint start, jint len)
{
    jint buf[64];
    (void)self;
    (*env)->GetIntArrayRegion(env, a, start, len, buf);
}

/* GetFieldID (isStatic false) / GetStaticFieldID (true) of the int field
 * `name` of `c`: whether an id came back; any exception is left pending. */
JNIEXPORT jboolean JNICALL
Java_GcfF2jJniRegionProbe_fieldId(JNIEnv *env, jclass self, jclass c, jstring name, jboolean isStatic)
{
    const char *n;
    jfieldID f;
    (void)self;
    n = (*env)->GetStringUTFChars(env, name, NULL);
    if (n == NULL) {
        return JNI_FALSE;
    }
    if (isStatic) {
        f = (*env)->GetStaticFieldID(env, c, n, "I");
    } else {
        f = (*env)->GetFieldID(env, c, n, "I");
    }
    (*env)->ReleaseStringUTFChars(env, name, n);
    return f != NULL ? JNI_TRUE : JNI_FALSE;
}

/* GetStringRegion(s, start, len); any exception is left pending. */
JNIEXPORT void JNICALL
Java_GcfF2jJniRegionProbe_stringRegion(JNIEnv *env, jclass self, jstring s, jint start, jint len)
{
    jchar buf[64];
    (void)self;
    (*env)->GetStringRegion(env, s, start, len, buf);
}
