/*
 * SPDX-License-Identifier: Apache-2.0
 * Copyright 2024-2026 Craton Software Company
 *
 * Native half of tools/probes/GcgG4jnJniBulkRegionProbe.java (gcg g4/jn,
 * 2026-10-02): large Get/Set<Type>ArrayRegion round trips, the case
 * `CRATONVM_JNI_BULK_ARRAY_COPY` exists for
 * (docs/known-issues/gc/common-w9g-proposal-bulk-jni-array-copies.md).
 * Build and run instructions are in the Java class comment.
 *
 * Each `roundTrip<T>` copies `src` out into a C buffer, flips one element
 * per round, and copies the buffer into `dst`, `rounds` times. No allocation,
 * no up-call, no blocking: only the two region functions per round.
 */
#include <jni.h>
#include <stdlib.h>

#define ROUND_TRIP(NAME, JTYPE, JARRAY, GET, SET, FLIP)                              \
    JNIEXPORT jint JNICALL                                                           \
    Java_GcgG4jnJniBulkRegionProbe_##NAME(JNIEnv *env, jclass self, JARRAY src,      \
                                          JARRAY dst, jint rounds)                   \
    {                                                                                \
        jsize len = (*env)->GetArrayLength(env, src);                                \
        JTYPE *buf;                                                                  \
        jint r;                                                                      \
        (void)self;                                                                  \
        if (len <= 0 || (*env)->GetArrayLength(env, dst) != len) {                   \
            return 1;                                                                \
        }                                                                            \
        buf = (JTYPE *)malloc((size_t)len * sizeof(JTYPE));                          \
        if (buf == NULL) {                                                           \
            return 2;                                                                \
        }                                                                            \
        for (r = 0; r < rounds; r++) {                                               \
            (*env)->GET(env, src, 0, len, buf);                                      \
            if ((*env)->ExceptionCheck(env)) {                                       \
                free(buf);                                                           \
                return 3;                                                            \
            }                                                                        \
            buf[r % len] = FLIP(buf[r % len], r);                                    \
            (*env)->SET(env, dst, 0, len, buf);                                      \
            if ((*env)->ExceptionCheck(env)) {                                       \
                free(buf);                                                           \
                return 4;                                                            \
            }                                                                        \
        }                                                                            \
        free(buf);                                                                   \
        return 0;                                                                    \
    }

#define FLIP_INT(v, r) ((jint)((v) ^ (jint)(r)))
#define FLIP_BYTE(v, r) ((jbyte)((v) ^ (jbyte)(r)))
#define FLIP_LONG(v, r) ((jlong)((v) ^ ((jlong)(r) << 33)))
#define FLIP_DOUBLE(v, r) ((jdouble)((v) + (jdouble)(r)))

ROUND_TRIP(roundTripInt, jint, jintArray, GetIntArrayRegion, SetIntArrayRegion, FLIP_INT)
ROUND_TRIP(roundTripByte, jbyte, jbyteArray, GetByteArrayRegion, SetByteArrayRegion, FLIP_BYTE)
ROUND_TRIP(roundTripLong, jlong, jlongArray, GetLongArrayRegion, SetLongArrayRegion, FLIP_LONG)
ROUND_TRIP(roundTripDouble, jdouble, jdoubleArray, GetDoubleArrayRegion, SetDoubleArrayRegion,
           FLIP_DOUBLE)
