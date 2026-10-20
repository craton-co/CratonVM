/*
 * SPDX-License-Identifier: Apache-2.0
 * Copyright 2024-2026 Craton Software Company
 *
 * Native half of tools/bench/GcfF1jJniSemanticsProbe.java (gcf f1/j,
 * 2026-10-01): JNI functions whose answers differed from HotSpot 25's --
 * GetVersion, GetModule, To/FromReflectedMethod, To/FromReflectedField and
 * GetEnv on a thread that never attached. Linux (pthreads). Build and run
 * instructions are in the Java class comment.
 */
#include <jni.h>
#include <pthread.h>
#include <stddef.h>

JNIEXPORT jint JNICALL
Java_GcfF1jJniSemanticsProbe_jniVersion(JNIEnv *env, jclass self)
{
    (void)self;
    return (*env)->GetVersion(env);
}

JNIEXPORT jobject JNICALL
Java_GcfF1jJniSemanticsProbe_module(JNIEnv *env, jclass self, jclass c)
{
    (void)self;
    return (*env)->GetModule(env, c);
}

/* NewStringUTF(GetStringUTFChars(s)): the modified UTF-8 round trip. */
JNIEXPORT jstring JNICALL
Java_GcfF1jJniSemanticsProbe_utfRoundTrip(JNIEnv *env, jclass self, jstring s)
{
    const char *utf;
    jstring back;
    (void)self;
    utf = (*env)->GetStringUTFChars(env, s, NULL);
    if (utf == NULL) {
        return NULL;
    }
    back = (*env)->NewStringUTF(env, utf);
    (*env)->ReleaseStringUTFChars(env, s, utf);
    return back;
}

/* ToReflectedMethod of the static `target(I)I` of `c`. */
JNIEXPORT jobject JNICALL
Java_GcfF1jJniSemanticsProbe_reflectedMethod(JNIEnv *env, jclass self, jclass c)
{
    jmethodID mid;
    (void)self;
    mid = (*env)->GetStaticMethodID(env, c, "target", "(I)I");
    if (mid == NULL) {
        return NULL;
    }
    return (*env)->ToReflectedMethod(env, c, mid, JNI_TRUE);
}

/* FromReflectedMethod of a REAL `Method` (from Java reflection), then a call
 * through the id: -1 when no id came back. */
JNIEXPORT jint JNICALL
Java_GcfF1jJniSemanticsProbe_callViaFromReflected(JNIEnv *env, jclass self, jobject m, jint x)
{
    jvalue arg;
    jmethodID mid = (*env)->FromReflectedMethod(env, m);
    if (mid == NULL) {
        return -1;
    }
    arg.i = x;
    return (*env)->CallStaticIntMethodA(env, self, mid, &arg);
}

/* ToReflectedField of the static `answer` (I) of `c`. */
JNIEXPORT jobject JNICALL
Java_GcfF1jJniSemanticsProbe_reflectedField(JNIEnv *env, jclass self, jclass c)
{
    jfieldID fid;
    (void)self;
    fid = (*env)->GetStaticFieldID(env, c, "answer", "I");
    if (fid == NULL) {
        return NULL;
    }
    return (*env)->ToReflectedField(env, c, fid, JNI_TRUE);
}

/* FromReflectedField of a REAL `Field`, then a read through the id: -1 when
 * no id came back. */
JNIEXPORT jint JNICALL
Java_GcfF1jJniSemanticsProbe_readViaFromReflected(JNIEnv *env, jclass self, jobject f)
{
    jfieldID fid = (*env)->FromReflectedField(env, f);
    if (fid == NULL) {
        return -1;
    }
    return (*env)->GetStaticIntField(env, self, fid);
}

static JavaVM *g_vm;
static jint g_rc;

static void *getenv_on_fresh_thread(void *unused)
{
    void *penv = (void *)0x1;
    (void)unused;
    g_rc = (*g_vm)->GetEnv(g_vm, &penv, JNI_VERSION_1_8);
    /* HotSpot also NULLs the out-parameter of a refused GetEnv. */
    if (g_rc != JNI_OK && penv != NULL) {
        g_rc = -100;
    }
    return NULL;
}

/* GetEnv from a pthread that never called AttachCurrentThread. HotSpot:
 * JNI_EDETACHED (-2). -98 / -99: the probe could not set itself up. */
JNIEXPORT jint JNICALL
Java_GcfF1jJniSemanticsProbe_getEnvOnFreshThread(JNIEnv *env, jclass self)
{
    pthread_t t;
    (void)self;
    if ((*env)->GetJavaVM(env, &g_vm) != JNI_OK || g_vm == NULL) {
        return -99;
    }
    if (pthread_create(&t, NULL, getenv_on_fresh_thread, NULL) != 0) {
        return -98;
    }
    pthread_join(t, NULL);
    return g_rc;
}
