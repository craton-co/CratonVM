/*
 * SPDX-License-Identifier: Apache-2.0
 * Copyright 2024-2026 Craton Software Company
 *
 * Native half of tools/probes/GciMf12JniSpecialSiteProbe.java (gci i12/mf,
 * 2026-10-13): private, final, private synchronized and throwing instance
 * natives reached through invokespecial / a devirtualized invokevirtual, a
 * private native of an abstract class, and a RegisterNatives rebind of the
 * private native mid-run.
 *
 * Windows: cl /LD /O1 /I%JAVA_HOME%\include /I%JAVA_HOME%\include\win32
 *          GciMf12JniSpecialSiteProbe.c /Fegcimf12.dll
 * Linux:   gcc -O1 -shared -fPIC -I$JAVA_HOME/include -I$JAVA_HOME/include/linux
 *          GciMf12JniSpecialSiteProbe.c -o libgcimf12.so
 */
#include <jni.h>

JNIEXPORT jint JNICALL
Java_GciMf12JniSpecialSiteProbe_00024P_priv(JNIEnv *env, jobject self, jint x)
{
    (void)env;
    (void)self;
    return x + 1;
}

JNIEXPORT jint JNICALL
Java_GciMf12JniSpecialSiteProbe_00024P_fin(JNIEnv *env, jobject self, jint x)
{
    (void)env;
    (void)self;
    return x + 2;
}

/* Private synchronized: asks Java whether this thread holds `this`. */
JNIEXPORT jboolean JNICALL
Java_GciMf12JniSpecialSiteProbe_00024P_held(JNIEnv *env, jobject self)
{
    jclass c = (*env)->GetObjectClass(env, self);
    jmethodID m = (*env)->GetMethodID(env, c, "heldSelf", "()Z");
    if (m == NULL) {
        return JNI_FALSE;
    }
    return (*env)->CallBooleanMethod(env, self, m);
}

/* Throws IllegalStateException("seven") when x == 7, else returns x. */
JNIEXPORT jint JNICALL
Java_GciMf12JniSpecialSiteProbe_00024P_thr(JNIEnv *env, jobject self, jint x)
{
    (void)self;
    if (x == 7) {
        jclass ise = (*env)->FindClass(env, "java/lang/IllegalStateException");
        if (ise != NULL) {
            (*env)->ThrowNew(env, ise, "seven");
        }
        return -1;
    }
    return x;
}

JNIEXPORT jint JNICALL
Java_GciMf12JniSpecialSiteProbe_00024A_apriv(JNIEnv *env, jobject self, jint x)
{
    (void)env;
    (void)self;
    return x * 3;
}

static jint JNICALL
priv_a(JNIEnv *env, jobject self, jint x)
{
    (void)env;
    (void)self;
    return x + 1;
}

static jint JNICALL
priv_b(JNIEnv *env, jobject self, jint x)
{
    (void)env;
    (void)self;
    return x + 100;
}

/* Rebinds `P.priv(I)I` by RegisterNatives: which = 0 -> x + 1, else x + 100. */
JNIEXPORT jint JNICALL
Java_GciMf12JniSpecialSiteProbe_rebind(JNIEnv *env, jclass self, jint which)
{
    JNINativeMethod m;
    jclass p;
    (void)self;
    p = (*env)->FindClass(env, "GciMf12JniSpecialSiteProbe$P");
    if (p == NULL) {
        return -1;
    }
    m.name = "priv";
    m.signature = "(I)I";
    m.fnPtr = which == 0 ? (void *)priv_a : (void *)priv_b;
    return (*env)->RegisterNatives(env, p, &m, 1);
}
