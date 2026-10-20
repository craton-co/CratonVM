// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// gcj j2/jn2: native half of tools/probes/Gcj2Jn2UpcallSelectProbe.java.
// Windows (MSVC x64 developer prompt, from tools/probes/jni):
//   cl /LD /O1 /I"%JAVA_HOME%\include" /I"%JAVA_HOME%\include\win32" Gcj2Jn2UpcallSelectProbe.c /Fegcj2jn2us.dll
// Linux:
//   cc -shared -fPIC -O1 -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//      Gcj2Jn2UpcallSelectProbe.c -o libgcj2jn2us.so

#include <jni.h>

static jmethodID lookup(JNIEnv *env, jclass clazz, jstring name, jstring sig) {
    const char *n = (*env)->GetStringUTFChars(env, name, NULL);
    const char *s = (*env)->GetStringUTFChars(env, sig, NULL);
    jmethodID mid = NULL;
    if (n != NULL && s != NULL) {
        mid = (*env)->GetMethodID(env, clazz, n, s);
    }
    if (n != NULL) {
        (*env)->ReleaseStringUTFChars(env, name, n);
    }
    if (s != NULL) {
        (*env)->ReleaseStringUTFChars(env, sig, s);
    }
    return mid;
}

JNIEXPORT jint JNICALL Java_Gcj2Jn2UpcallSelectProbe_callInt(
    JNIEnv *env, jclass self, jobject recv, jclass clazz, jstring name, jstring sig) {
    (void)self;
    jmethodID mid = lookup(env, clazz, name, sig);
    if (mid == NULL) {
        (*env)->ExceptionClear(env);
        return -1;
    }
    return (*env)->CallIntMethod(env, recv, mid);
}

JNIEXPORT jint JNICALL Java_Gcj2Jn2UpcallSelectProbe_callIntA(
    JNIEnv *env, jclass self, jobject recv, jclass clazz, jstring name, jstring sig) {
    (void)self;
    jmethodID mid = lookup(env, clazz, name, sig);
    if (mid == NULL) {
        (*env)->ExceptionClear(env);
        return -1;
    }
    return (*env)->CallIntMethodA(env, recv, mid, NULL);
}
