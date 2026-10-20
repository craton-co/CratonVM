/*
 * SPDX-License-Identifier: Apache-2.0
 * Copyright 2024-2026 Craton Software Company
 *
 * Native half of tools/bench/R16JniUsleepProbe.java (JIT round 16 wave 11,
 * lane jnieintr). Linux. Build and run instructions are in the Java class
 * comment.
 *
 * Each function blocks once in a syscall that a signal handler interrupts
 * whatever its SA_RESTART flag (signal(7)): `usleep` (nanosleep) and `poll`.
 * They return 0, or -errno when the call failed (EINTR = -4), and never retry:
 * the probe counts what a JNI native in the wild would see.
 */
#include <jni.h>
#include <errno.h>
#include <poll.h>
#include <unistd.h>

JNIEXPORT jint JNICALL
Java_R16JniUsleepProbe_sleepOnce(JNIEnv *env, jclass self, jint us)
{
    (void)env;
    (void)self;
    if (usleep((useconds_t)us) == 0) {
        return 0;
    }
    return -errno;
}

JNIEXPORT jint JNICALL
Java_R16JniUsleepProbe_pollOnce(JNIEnv *env, jclass self, jint ms)
{
    (void)env;
    (void)self;
    if (poll(NULL, 0, ms) >= 0) {
        return 0;
    }
    return -errno;
}
