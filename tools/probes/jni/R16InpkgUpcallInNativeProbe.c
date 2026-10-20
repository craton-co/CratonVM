/*
 * SPDX-License-Identifier: Apache-2.0
 * Copyright 2024-2026 Craton Software Company
 *
 * Native half of tools/bench/R16InpkgUpcallInNativeProbe.java (JIT round 16
 * wave 12, lane inpkg). Linux. Build and run instructions are in the Java
 * class comment.
 *
 * `callBack` is a JNI native whose C code calls an FFM up-call stub `n` times
 * (the stub's address arrives as a jlong) and sums what it answers.
 * `ffmEntry` is a plain C function the Java side calls through an FFM
 * downcall; it calls an up-call stub once, whose Java then calls `callBack`
 * -- a JNI native inside an FFM downcall's up-call.
 */
#include <jni.h>
#include <stdint.h>

typedef jlong (*upcall_fn)(jint);

JNIEXPORT jlong JNICALL
Java_R16InpkgUpcallInNativeProbe_callBack(JNIEnv *env, jclass self, jlong fn, jint n)
{
    (void)env;
    (void)self;
    upcall_fn f = (upcall_fn)(intptr_t)fn;
    jlong sum = 0;
    for (jint i = 0; i < n; i++) {
        sum += f(i);
    }
    return sum;
}

typedef jlong (*outer_fn)(jint);

/* Called through an FFM downcall: one up-call, whose answer is returned. */
jlong ffmEntry(void *fn, jint n)
{
    outer_fn f = (outer_fn)fn;
    return f(n);
}
