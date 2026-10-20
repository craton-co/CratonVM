/*
 * SPDX-License-Identifier: Apache-2.0
 * Copyright 2024-2026 Craton Software Company
 *
 * Native half of tools/bench/GcfF2jJniRepeatProbe.java (gcf f2/j,
 * 2026-10-01): two JNI natives a compiled loop calls from two call sites,
 * each reading the array argument through JNIEnv, so a stale or missing root
 * reads wrong values. Build and run instructions are in the Java class
 * comment.
 */
#include <jni.h>

/* a[i % 3] through GetIntArrayRegion; -1 when the array is short. */
JNIEXPORT jint JNICALL
Java_GcfF2jJniRepeatProbe_pick(JNIEnv *env, jclass self, jintArray a, jint i)
{
    jint v[3];
    (void)self;
    if ((*env)->GetArrayLength(env, a) < 3) {
        return -1;
    }
    (*env)->GetIntArrayRegion(env, a, 0, 3, v);
    return v[i % 3];
}

/* The length of s's class name, through GetObjectClass + a String up-call
 * (a native that escalates: a local, an up-call, a pending-exception check). */
JNIEXPORT jint JNICALL
Java_GcfF2jJniRepeatProbe_nameLength(JNIEnv *env, jclass self, jobject o)
{
    jclass cls;
    jclass classClass;
    jmethodID getName;
    jstring name;
    jint len;
    (void)self;
    cls = (*env)->GetObjectClass(env, o);
    classClass = (*env)->GetObjectClass(env, cls);
    getName = (*env)->GetMethodID(env, classClass, "getName", "()Ljava/lang/String;");
    if (getName == NULL) {
        return -1;
    }
    name = (jstring)(*env)->CallObjectMethod(env, cls, getName);
    if (name == NULL) {
        return -2;
    }
    len = (*env)->GetStringLength(env, name);
    (*env)->DeleteLocalRef(env, name);
    (*env)->DeleteLocalRef(env, classClass);
    (*env)->DeleteLocalRef(env, cls);
    return len;
}
