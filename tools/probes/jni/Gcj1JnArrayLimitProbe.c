/*
 * SPDX-License-Identifier: Apache-2.0
 * Copyright 2024-2026 Craton Software Company
 *
 * Native half of tools/probes/Gcj1JnArrayLimitProbe.java (gcj j1/jn,
 * 2026-10-19; `docs/known-issues/gc/gcj-j1jn-jni-array-past-the-vm-limit-is-java-heap-space-20261019.md`).
 * Each function makes one JNI array request of `len` elements and returns
 * what JNI returned; a refused request returns NULL with the error pending,
 * which the JVM throws when the native returns.
 *
 * Build (Windows, MSVC x64 developer prompt, Temurin 25.0.3 headers):
 *   cl /LD /O1 /I"%JAVA_HOME%\include" /I"%JAVA_HOME%\include\win32" Gcj1JnArrayLimitProbe.c /Fegcj1jnal.dll
 * Build (Linux):
 *   gcc -O1 -shared -fPIC -I"$JDK/include" -I"$JDK/include/linux" \
 *       -o libgcj1jnal.so tools/probes/jni/Gcj1JnArrayLimitProbe.c
 */
#include <jni.h>

JNIEXPORT jobject JNICALL Java_Gcj1JnArrayLimitProbe_newByteArray(JNIEnv *env, jclass cls, jint len)
{
    (void)cls;
    return (*env)->NewByteArray(env, len);
}

JNIEXPORT jobject JNICALL Java_Gcj1JnArrayLimitProbe_newLongArray(JNIEnv *env, jclass cls, jint len)
{
    (void)cls;
    return (*env)->NewLongArray(env, len);
}

JNIEXPORT jobject JNICALL Java_Gcj1JnArrayLimitProbe_newObjectArray(JNIEnv *env, jclass cls, jint len,
                                                                   jclass component)
{
    (void)cls;
    return (*env)->NewObjectArray(env, len, component, NULL);
}
