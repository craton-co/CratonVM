/*
 * SPDX-License-Identifier: Apache-2.0
 * Copyright 2024-2026 Craton Software Company
 *
 * Native half of tools/probes/GciDm8JniMemoProbe.java (gci i8/dm, 2026-10-13):
 * the shapes the per-thread JNI dispatch memo (CRATONVM_JNI_DISPATCH_MEMO)
 * must serve exactly as the full by-name path does -- a dlsym-bound static,
 * synchronized static and instance natives (monitor held, upcall), a
 * RegisterNatives rebind mid-run (the registration generation), a pending
 * exception, an inherited and an overridden instance native, an abstract
 * class's native called through a concrete subclass.
 *
 * Windows: cl /LD /O1 /I%JAVA_HOME%\include /I%JAVA_HOME%\include\win32
 *          GciDm8JniMemoProbe.c /Fegcidm8jnimemo.dll
 */
#include <jni.h>

JNIEXPORT jint JNICALL
Java_GciDm8JniMemoProbe_add1(JNIEnv *env, jclass self, jint x)
{
    (void)env;
    (void)self;
    return x + 1;
}

/* Static synchronized: asks Java whether this thread holds the class's
 * monitor (HotSpot: always true). */
JNIEXPORT jboolean JNICALL
Java_GciDm8JniMemoProbe_syncHeld(JNIEnv *env, jclass self)
{
    jmethodID m = (*env)->GetStaticMethodID(env, self, "held", "()Z");
    if (m == NULL) {
        return JNI_FALSE;
    }
    return (*env)->CallStaticBooleanMethod(env, self, m);
}

/* Static synchronized: a non-atomic Java increment under the class monitor. */
JNIEXPORT void JNICALL
Java_GciDm8JniMemoProbe_syncBump(JNIEnv *env, jclass self)
{
    jmethodID m = (*env)->GetStaticMethodID(env, self, "bump", "()V");
    if (m != NULL) {
        (*env)->CallStaticVoidMethod(env, self, m);
    }
}

static jint JNICALL
swap_a(JNIEnv *env, jclass self, jint x)
{
    (void)env;
    (void)self;
    return x + 1;
}

static jint JNICALL
swap_b(JNIEnv *env, jclass self, jint x)
{
    (void)env;
    (void)self;
    return x + 2;
}

/* Binds `swapped(I)I` by RegisterNatives: which = 0 -> x + 1, else x + 2. */
JNIEXPORT jint JNICALL
Java_GciDm8JniMemoProbe_bind(JNIEnv *env, jclass self, jint which)
{
    JNINativeMethod m;
    m.name = "swapped";
    m.signature = "(I)I";
    m.fnPtr = which == 0 ? (void *)swap_a : (void *)swap_b;
    return (*env)->RegisterNatives(env, self, &m, 1);
}

/* Throws IllegalStateException("seven") when x == 7, else returns x. */
JNIEXPORT jint JNICALL
Java_GciDm8JniMemoProbe_throwIf(JNIEnv *env, jclass self, jint x)
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
Java_GciDm8JniMemoProbe_00024Base_instAdd(JNIEnv *env, jobject self, jint x)
{
    (void)env;
    (void)self;
    return x + 10;
}

/* Instance synchronized: asks Java whether this thread holds `this`. */
JNIEXPORT jboolean JNICALL
Java_GciDm8JniMemoProbe_00024Base_instHeld(JNIEnv *env, jobject self)
{
    jclass c = (*env)->GetObjectClass(env, self);
    jmethodID m = (*env)->GetMethodID(env, c, "heldSelf", "()Z");
    if (m == NULL) {
        return JNI_FALSE;
    }
    return (*env)->CallBooleanMethod(env, self, m);
}

JNIEXPORT jint JNICALL
Java_GciDm8JniMemoProbe_00024Abs_absGet(JNIEnv *env, jobject self, jint x)
{
    (void)env;
    (void)self;
    return x * 2;
}
