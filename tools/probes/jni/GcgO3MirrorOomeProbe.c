// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// gcg g3/o (2026-10-01): native half of tools/probes/GcgO3MirrorOomeProbe.java.
// The same fill as GcgO1JniOomeProbe.c, bound to this probe's class: `fill`
// exhausts the heap from C with JNI `NewByteArray`, holding every array
// through a global reference, and returns with the refused allocation's
// `OutOfMemoryError` pending. `release` drops every global reference.
//
//   gcc -O1 -shared -fPIC -I"$JDK/include" -I"$JDK/include/linux" \
//       -o /tmp/libgcgo3mirroroome.so tools/probes/jni/GcgO3MirrorOomeProbe.c

#include <jni.h>
#include <stdlib.h>

static jobject *held;
static int nheld;
static int cap;

JNIEXPORT jint JNICALL Java_GcgO3MirrorOomeProbe_fill(JNIEnv *env, jclass cls, jint chunk) {
    (void)cls;
    for (;;) {
        jbyteArray a = (*env)->NewByteArray(env, chunk);
        if (a == NULL) {
            // A smaller chunk first, so the heap ends as full as it can get;
            // the last refusal's OutOfMemoryError is left pending.
            if (chunk > 64) {
                (*env)->ExceptionClear(env);
                chunk /= 4;
                continue;
            }
            return nheld;
        }
        if (nheld == cap) {
            int next = cap ? cap * 2 : 1024;
            jobject *grown = (jobject *)realloc(held, (size_t)next * sizeof(jobject));
            if (grown == NULL) {
                abort();
            }
            held = grown;
            cap = next;
        }
        jobject g = (*env)->NewGlobalRef(env, a);
        (*env)->DeleteLocalRef(env, a);
        if (g == NULL) {
            return nheld;
        }
        held[nheld++] = g;
    }
}

JNIEXPORT jint JNICALL Java_GcgO3MirrorOomeProbe_release(JNIEnv *env, jclass cls) {
    (void)cls;
    int n = nheld;
    for (int i = 0; i < nheld; i++) {
        (*env)->DeleteGlobalRef(env, held[i]);
    }
    nheld = 0;
    return n;
}
