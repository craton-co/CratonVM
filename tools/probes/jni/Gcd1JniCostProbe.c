/*
 * SPDX-License-Identifier: Apache-2.0
 * Copyright 2024-2026 Craton Software Company
 *
 * Native half of tools/bench/Gcd1JniCostProbe.java (gcd d4/k, 2026-09-28):
 * the per-call cost of a JNI native dispatch and of the JNIEnv calls a native
 * makes, for the flip gate of CRATONVM_JNI_INDIRECT_LOCALS +
 * CRATONVM_JNI_NATIVE_TRANSITIONS. Each loop returns a checksum the Java side
 * verifies, so the loop cannot be elided and a wrong answer is caught.
 */
#include <jni.h>

/* The empty native: the dispatch (and, with the flags on, the in-native
 * bracket) and nothing else. */
JNIEXPORT jint JNICALL
Java_Gcd1JniCostProbe_noop(JNIEnv *env, jclass self, jint x)
{
    (void)env;
    (void)self;
    return x + 1;
}

/* n x GetArrayLength: the cheapest JNIEnv call that decodes a local. */
JNIEXPORT jlong JNICALL
Java_Gcd1JniCostProbe_loopArrayLength(JNIEnv *env, jclass self, jintArray a, jint n)
{
    jlong sum = 0;
    jint i;
    (void)self;
    for (i = 0; i < n; i++) {
        sum += (*env)->GetArrayLength(env, a);
    }
    return sum;
}

/* n x GetIntArrayRegion of one element: the region-copy loop shape. */
JNIEXPORT jlong JNICALL
Java_Gcd1JniCostProbe_loopRegion(JNIEnv *env, jclass self, jintArray a, jint n)
{
    jlong sum = 0;
    jint i;
    jint len = (*env)->GetArrayLength(env, a);
    (void)self;
    for (i = 0; i < n; i++) {
        jint v = 0;
        (*env)->GetIntArrayRegion(env, a, i % len, 1, &v);
        sum += v;
    }
    return sum;
}

/* n x (NewStringUTF + GetStringUTFLength + DeleteLocalRef): an allocating
 * JNIEnv call that mints a local, and its release. */
JNIEXPORT jlong JNICALL
Java_Gcd1JniCostProbe_loopNewString(JNIEnv *env, jclass self, jint n)
{
    jlong sum = 0;
    jint i;
    (void)self;
    for (i = 0; i < n; i++) {
        jstring s = (*env)->NewStringUTF(env, "cost");
        if (s == NULL) {
            return -1;
        }
        sum += (*env)->GetStringUTFLength(env, s);
        (*env)->DeleteLocalRef(env, s);
    }
    return sum;
}
