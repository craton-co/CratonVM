/* SPDX-License-Identifier: Apache-2.0 */
/* Copyright 2024-2026 Craton Software Company */
#include <jni.h>

JNIEXPORT jobject JNICALL Java_GcReferentReadKeepAliveProbe_readReferent(
    JNIEnv *env, jclass unused, jobject reference) {
    /* Resolve through the receiver subclass, just as JNI applications do. */
    jclass child = (*env)->GetObjectClass(env, reference);
    jfieldID field = (*env)->GetFieldID(env, child, "referent", "Ljava/lang/Object;");
    if (field == NULL) return NULL;
    jobject result = (*env)->GetObjectField(env, reference, field);
    (*env)->DeleteLocalRef(env, child);
    return result;
}
