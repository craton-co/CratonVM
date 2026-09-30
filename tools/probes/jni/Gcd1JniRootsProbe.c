/*
 * SPDX-License-Identifier: Apache-2.0
 * Copyright 2024-2026 Craton Software Company
 *
 * Native half of tools/bench/Gcd1JniRootsProbe.java (gcd d2/i, 2026-09-27).
 * Linux (pthreads) only. Build and run instructions are in the Java class
 * comment. Every JNI call uses the `A` (jvalue array) form: no C varargs.
 *
 * Nothing here blocks inside a native method while another thread may need a
 * collection: `foreignStart` returns at once, and `foreignJoin` is called only
 * after `foreignDetached` said the attached thread makes no further VM call.
 * (A Java thread inside a JNI native is a COUNTED mutator in CratonVM, so a
 * native that waited on a thread whose up-calls collect would hold every
 * pause: `docs/known-issues/gc/gcd-d2i-jni-native-methods-are-counted-mutators-20260927.md`.)
 */
#include <jni.h>
#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

/* ---- Same-thread cases --------------------------------------------------- */

/*
 * `o` is a local ref. Take a global ref to the same object, run the
 * allocating up-call, then ask whether the native's own copy of `o` still
 * names that object and still reads the field. 0 = both hold; bit 0 =
 * IsSameObject(o, g) false; bit 1 = the int field read back wrong; bit 2 = an
 * exception was pending after the up-call.
 */
JNIEXPORT jint JNICALL
Java_Gcd1JniRootsProbe_localAcrossUpcall(JNIEnv *env, jclass self, jobject o, jobject churn,
                                         jint expected)
{
    jint code = 0;
    jobject g = (*env)->NewGlobalRef(env, o);
    jclass rc = (*env)->GetObjectClass(env, churn);
    jmethodID run = (*env)->GetMethodID(env, rc, "run", "()V");
    jclass hc = (*env)->GetObjectClass(env, o);
    jfieldID fv = (*env)->GetFieldID(env, hc, "v", "I");
    (void)self;
    (*env)->CallVoidMethodA(env, churn, run, NULL);
    if ((*env)->ExceptionCheck(env)) {
        (*env)->ExceptionClear(env);
        code |= 4;
    }
    if (!(*env)->IsSameObject(env, o, g)) {
        code |= 1;
    } else if ((*env)->GetIntField(env, o, fv) != expected) {
        code |= 2;
    }
    (*env)->DeleteGlobalRef(env, g);
    return code;
}

/*
 * MonitorEnter through the local `o`, the allocating up-call, MonitorExit
 * through the same local. 0 = both succeeded; 1 = MonitorExit failed or left
 * an exception pending (cleared here); 2 = MonitorEnter failed.
 */
JNIEXPORT jint JNICALL
Java_Gcd1JniRootsProbe_monitorAcrossUpcall(JNIEnv *env, jclass self, jobject o, jobject churn)
{
    jclass rc = (*env)->GetObjectClass(env, churn);
    jmethodID run = (*env)->GetMethodID(env, rc, "run", "()V");
    (void)self;
    if ((*env)->MonitorEnter(env, o) != JNI_OK) {
        return 2;
    }
    (*env)->CallVoidMethodA(env, churn, run, NULL);
    if ((*env)->ExceptionCheck(env)) {
        (*env)->ExceptionClear(env);
    }
    if ((*env)->MonitorExit(env, o) != JNI_OK || (*env)->ExceptionCheck(env)) {
        (*env)->ExceptionClear(env);
        return 1;
    }
    return 0;
}

/* ---- The attached host thread -------------------------------------------- */

static JavaVM *g_vm;
static jobject g_cb;         /* global ref to the probe's callback object */
static jmethodID g_accept;   /* void accept(String, byte[], int) */
static jmethodID g_make;     /* String make(int) */
static jmethodID g_churn;    /* void churnOnce() */
static jmethodID g_check;    /* boolean checkResult(Object, int) */
static jmethodID g_done;     /* void done() */
static int g_iters;
static pthread_t g_thread;
static int g_started;
/* Set by the attached thread after DetachCurrentThread returned: from then on
 * it makes no VM call, so a pthread_join cannot wait on a pause. */
static int g_detached;

/* Counters written by the attached thread, read by foreignJoin after the join. */
static int c_attach_bad;     /* AttachCurrentThread failed */
static int c_locals_bad;     /* a local made at idle no longer names its object */
static int c_results_bad;    /* a Call*ObjectMethod result held across an up-call */
static int c_arrays_bad;     /* Get/ReleaseIntArrayElements across idle windows */
static int c_exceptions;     /* an up-call left an exception pending */

static void clear_pending(JNIEnv *env)
{
    if ((*env)->ExceptionCheck(env)) {
        (*env)->ExceptionClear(env);
        c_exceptions++;
    }
}

static void *foreign_main(void *arg)
{
    JNIEnv *env = NULL;
    int i;
    (void)arg;
    if ((*g_vm)->AttachCurrentThread(g_vm, (void **)&env, NULL) != JNI_OK || env == NULL) {
        c_attach_bad = 1;
        __atomic_store_n(&g_detached, 1, __ATOMIC_RELEASE);
        return NULL;
    }
    for (i = 0; i < g_iters; i++) {
        char msg[32];
        jbyte bytes[64];
        jint ints[256];
        jvalue a3[3];
        jvalue a2[2];
        jvalue a1[1];
        int k;

        /* 1. Locals made while idle (GC-blocked between JNI functions), used
         *    after an idle window in which the Java churn collects. */
        snprintf(msg, sizeof msg, "m%d", i);
        for (k = 0; k < 64; k++) {
            bytes[k] = (jbyte)(i + k);
        }
        jstring s = (*env)->NewStringUTF(env, msg);
        jbyteArray b = (*env)->NewByteArray(env, 64);
        if (s == NULL || b == NULL) {
            clear_pending(env);
            c_locals_bad++;
            continue;
        }
        (*env)->SetByteArrayRegion(env, b, 0, 64, bytes);
        jobject gs = (*env)->NewGlobalRef(env, s);
        jobject gb = (*env)->NewGlobalRef(env, b);
        usleep(200);
        if (!(*env)->IsSameObject(env, s, gs) || !(*env)->IsSameObject(env, b, gb)) {
            c_locals_bad++;
        } else {
            a3[0].l = s;
            a3[1].l = b;
            a3[2].i = i;
            (*env)->CallVoidMethodA(env, g_cb, g_accept, a3);
            clear_pending(env);
        }
        (*env)->DeleteGlobalRef(env, gs);
        (*env)->DeleteGlobalRef(env, gb);
        (*env)->DeleteLocalRef(env, s);
        (*env)->DeleteLocalRef(env, b);

        /* 2. An object result held (no global ref) across a second, allocating
         *    up-call. */
        a1[0].i = i;
        jobject r = (*env)->CallObjectMethodA(env, g_cb, g_make, a1);
        clear_pending(env);
        (*env)->CallVoidMethodA(env, g_cb, g_churn, NULL);
        clear_pending(env);
        a2[0].l = r;
        a2[1].i = i;
        if (r == NULL || !(*env)->CallBooleanMethodA(env, g_cb, g_check, a2)) {
            c_results_bad++;
        }
        clear_pending(env);
        if (r != NULL) {
            (*env)->DeleteLocalRef(env, r);
        }

        /* 3. Element copies of one array across idle windows. */
        if (i % 8 == 0) {
            jintArray arr = (*env)->NewIntArray(env, 256);
            if (arr == NULL) {
                clear_pending(env);
                c_arrays_bad++;
                continue;
            }
            for (k = 0; k < 256; k++) {
                ints[k] = i * 256 + k;
            }
            (*env)->SetIntArrayRegion(env, arr, 0, 256, ints);
            jobject ga = (*env)->NewGlobalRef(env, arr);
            int rep;
            for (rep = 0; rep < 4; rep++) {
                usleep(100);
                if (!(*env)->IsSameObject(env, arr, ga)) {
                    c_arrays_bad++;
                    break;
                }
                jint *e = (*env)->GetIntArrayElements(env, arr, NULL);
                if (e == NULL) {
                    clear_pending(env);
                    c_arrays_bad++;
                    break;
                }
                long long sum = 0;
                for (k = 0; k < 256; k++) {
                    sum += e[k];
                }
                (*env)->ReleaseIntArrayElements(env, arr, e, JNI_ABORT);
                if (sum != (long long)i * 256 * 256 + 255LL * 256 / 2) {
                    c_arrays_bad++;
                    break;
                }
            }
            (*env)->DeleteGlobalRef(env, ga);
            (*env)->DeleteLocalRef(env, arr);
        }
    }
    (*env)->CallVoidMethodA(env, g_cb, g_done, NULL);
    clear_pending(env);
    (*g_vm)->DetachCurrentThread(g_vm);
    __atomic_store_n(&g_detached, 1, __ATOMIC_RELEASE);
    return NULL;
}

/* Has the attached thread detached? Never blocks. */
JNIEXPORT jboolean JNICALL
Java_Gcd1JniRootsProbe_foreignDetached(JNIEnv *env, jclass self)
{
    (void)env;
    (void)self;
    return __atomic_load_n(&g_detached, __ATOMIC_ACQUIRE) ? JNI_TRUE : JNI_FALSE;
}

/* Start the attached thread; 0 = started. Returns at once. */
JNIEXPORT jint JNICALL
Java_Gcd1JniRootsProbe_foreignStart(JNIEnv *env, jclass self, jobject cb, jint iters)
{
    jclass cc;
    (void)self;
    if ((*env)->GetJavaVM(env, &g_vm) != JNI_OK) {
        return 1;
    }
    g_cb = (*env)->NewGlobalRef(env, cb);
    cc = (*env)->GetObjectClass(env, cb);
    g_accept = (*env)->GetMethodID(env, cc, "accept", "(Ljava/lang/String;[BI)V");
    g_make = (*env)->GetMethodID(env, cc, "make", "(I)Ljava/lang/String;");
    g_churn = (*env)->GetMethodID(env, cc, "churnOnce", "()V");
    g_check = (*env)->GetMethodID(env, cc, "checkResult", "(Ljava/lang/Object;I)Z");
    g_done = (*env)->GetMethodID(env, cc, "done", "()V");
    if (g_cb == NULL || g_accept == NULL || g_make == NULL || g_churn == NULL || g_check == NULL
        || g_done == NULL) {
        (*env)->ExceptionClear(env);
        return 2;
    }
    g_iters = iters;
    if (pthread_create(&g_thread, NULL, foreign_main, NULL) != 0) {
        return 3;
    }
    g_started = 1;
    return 0;
}

/* Join the attached thread (already detached, see foreignDetached) and hand
 * back its counters as [attach, locals, results, arrays, exceptions]. */
JNIEXPORT void JNICALL
Java_Gcd1JniRootsProbe_foreignJoin(JNIEnv *env, jclass self, jintArray out)
{
    jint v[5];
    (void)self;
    if (g_started) {
        pthread_join(g_thread, NULL);
        g_started = 0;
    }
    if (g_cb != NULL) {
        (*env)->DeleteGlobalRef(env, g_cb);
        g_cb = NULL;
    }
    v[0] = c_attach_bad;
    v[1] = c_locals_bad;
    v[2] = c_results_bad;
    v[3] = c_arrays_bad;
    v[4] = c_exceptions;
    (*env)->SetIntArrayRegion(env, out, 0, 5, v);
}
