/*
 * SPDX-License-Identifier: Apache-2.0
 * Copyright 2024-2026 Craton Software Company
 *
 * Native half of tools/bench/Gcd1JniRootsProbe.java (gcd d2/i, 2026-09-27).
 * Linux (pthreads); Windows through the small shim below (gc JNI retirement,
 * 2026-10-10: `cl /LD /O1 /I"%JDK%\include" /I"%JDK%\include\win32"
 * Gcd1JniRootsProbe.c /Fegcd1jniroots.dll`). Build and run instructions are in
 * the Java class comment. Every JNI call uses the `A` (jvalue array) form: no C varargs.
 *
 * Nothing here blocks inside a native method while another thread may need a
 * collection: `foreignStart` returns at once, and `foreignJoin` is called only
 * after `foreignDetached` said the attached thread makes no further VM call.
 * (A Java thread inside a JNI native is a COUNTED mutator in CratonVM, so a
 * native that waited on a thread whose up-calls collect would hold every
 * pause: `docs/internal/proposals/gc/gcd-d2i-jni-native-methods-are-counted-mutators-CLOSED-USER-DECISION-20261012.md`.)
 */
#include <jni.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#ifdef _WIN32
#include <windows.h>
typedef HANDLE pthread_t;
static DWORD WINAPI win_thread_main(LPVOID arg);
static void *(*win_thread_fn)(void *);
static int pthread_create(pthread_t *t, void *attr, void *(*fn)(void *), void *arg)
{
    (void)attr;
    win_thread_fn = fn;
    *t = CreateThread(NULL, 0, win_thread_main, arg, 0, NULL);
    return *t == NULL;
}
static DWORD WINAPI win_thread_main(LPVOID arg)
{
    win_thread_fn(arg);
    return 0;
}
static int pthread_join(pthread_t t, void **ret)
{
    (void)ret;
    WaitForSingleObject(t, INFINITE);
    CloseHandle(t);
    return 0;
}
/*
 * gci i2/jn (gci-i1orch-windows-jni-foreign-attach-hangs-20261012): `Sleep(1)`
 * waits for the process's timer tick, 15.625 ms unless the process asked for
 * a finer one (`timeBeginPeriod`); a 200 us idle window then costs 78x its
 * Linux length, and the 20000-iteration run (about 30000 sleeps) needs >= 468 s
 * on a process at the default tick, past the probe's own 300 s deadline. A
 * high-resolution waitable timer (Windows 10 1803+) sleeps about the asked
 * time whatever the process tick. `GCD1_JNI_TICK_SLEEP=1` keeps the old
 * `Sleep` (the fail-first arm); a timer that cannot be made falls back to it.
 */
#ifndef CREATE_WAITABLE_TIMER_HIGH_RESOLUTION
#define CREATE_WAITABLE_TIMER_HIGH_RESOLUTION 0x00000002
#endif
static HANDLE g_sleep_timer;
static int g_tick_sleep = -1;
static void usleep(unsigned us)
{
    LARGE_INTEGER due;
    if (g_tick_sleep < 0) {
        const char *v = getenv("GCD1_JNI_TICK_SLEEP");
        g_tick_sleep = v != NULL && v[0] == '1';
        if (!g_tick_sleep) {
            g_sleep_timer = CreateWaitableTimerExW(NULL, NULL,
                                                   CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                                                   TIMER_ALL_ACCESS);
            if (g_sleep_timer == NULL) {
                g_tick_sleep = 1;
            }
        }
    }
    if (g_tick_sleep) {
        Sleep(us >= 1000 ? us / 1000 : 1);
        return;
    }
    due.QuadPart = -(LONGLONG)us * 10; /* relative, 100 ns units */
    if (!SetWaitableTimer(g_sleep_timer, &due, 0, NULL, NULL, FALSE)) {
        Sleep(us >= 1000 ? us / 1000 : 1);
        return;
    }
    WaitForSingleObject(g_sleep_timer, INFINITE);
}
static unsigned long long now_us(void)
{
    static LARGE_INTEGER freq;
    LARGE_INTEGER t;
    if (freq.QuadPart == 0) {
        QueryPerformanceFrequency(&freq);
    }
    QueryPerformanceCounter(&t);
    return (unsigned long long)(t.QuadPart / freq.QuadPart) * 1000000ULL
        + (unsigned long long)(t.QuadPart % freq.QuadPart) * 1000000ULL
              / (unsigned long long)freq.QuadPart;
}
#define ATOMIC_STORE_RELEASE(p, v) InterlockedExchange((volatile LONG *)(p), (v))
#define ATOMIC_LOAD_ACQUIRE(p) InterlockedCompareExchange((volatile LONG *)(p), 0, 0)
#else
#include <pthread.h>
#include <time.h>
#include <unistd.h>
static unsigned long long now_us(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (unsigned long long)ts.tv_sec * 1000000ULL + (unsigned long long)ts.tv_nsec / 1000ULL;
}
#define ATOMIC_STORE_RELEASE(p, v) __atomic_store_n((p), (v), __ATOMIC_RELEASE)
#define ATOMIC_LOAD_ACQUIRE(p) __atomic_load_n((p), __ATOMIC_ACQUIRE)
#endif

/*
 * gci i2/jn: where the attached thread's time goes, on stderr only (stdout is
 * the HotSpot diff). `idle_sleep` wraps every idle-window sleep; foreign_main
 * prints a progress line every 2000 iterations and a summary at the end:
 *   [gcd1-roots] foreign iter=<i> wall_ms=<w> sleep_ms=<s> sleeps=<n>
 *   [gcd1-roots] foreign done iters=<n> wall_ms=<w> sleep_ms=<s> sleeps=<k> us_per_sleep=<u>
 * A run that stops printing progress is a hang at that iteration; one whose
 * sleep_ms is most of wall_ms is the timer tick, not the VM.
 * gci i12/fs: `GCD1_JNI_PROGRESS_EVERY=<n>` (n > 0) prints the progress line
 * every n iterations instead of 2000, so a slow row (the roster audit on
 * Generational) shows whether the attached thread still advances near the
 * probe's deadline. stdout is unchanged.
 */
static unsigned long long g_sleep_us;
static unsigned long long g_sleeps;
static void idle_sleep(unsigned us)
{
    unsigned long long t0 = now_us();
    usleep(us);
    g_sleep_us += now_us() - t0;
    g_sleeps++;
}

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
    int every = 2000;
    const char *every_env = getenv("GCD1_JNI_PROGRESS_EVERY");
    unsigned long long t_start = now_us();
    (void)arg;
    if (every_env != NULL && atoi(every_env) > 0) {
        every = atoi(every_env);
    }
    fprintf(stderr, "[gcd1-roots] foreign attaching\n");
    fflush(stderr);
    if ((*g_vm)->AttachCurrentThread(g_vm, (void **)&env, NULL) != JNI_OK || env == NULL) {
        c_attach_bad = 1;
        ATOMIC_STORE_RELEASE(&g_detached, 1);
        return NULL;
    }
    fprintf(stderr, "[gcd1-roots] foreign attached attach_us=%llu\n", now_us() - t_start);
    fflush(stderr);
    for (i = 0; i < g_iters; i++) {
        char msg[32];
        jbyte bytes[64];
        jint ints[256];
        jvalue a3[3];
        jvalue a2[2];
        jvalue a1[1];
        int k;

        if (i > 0 && i % every == 0) {
            fprintf(stderr, "[gcd1-roots] foreign iter=%d wall_ms=%llu sleep_ms=%llu sleeps=%llu\n",
                    i, (now_us() - t_start) / 1000ULL, g_sleep_us / 1000ULL, g_sleeps);
            fflush(stderr);
        }

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
        idle_sleep(200);
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
                idle_sleep(100);
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
    fprintf(stderr,
            "[gcd1-roots] foreign done iters=%d wall_ms=%llu sleep_ms=%llu sleeps=%llu"
            " us_per_sleep=%llu\n",
            g_iters, (now_us() - t_start) / 1000ULL, g_sleep_us / 1000ULL, g_sleeps,
            g_sleeps ? g_sleep_us / g_sleeps : 0ULL);
    fflush(stderr);
    (*env)->CallVoidMethodA(env, g_cb, g_done, NULL);
    clear_pending(env);
    (*g_vm)->DetachCurrentThread(g_vm);
    ATOMIC_STORE_RELEASE(&g_detached, 1);
    return NULL;
}

/* Has the attached thread detached? Never blocks. */
JNIEXPORT jboolean JNICALL
Java_Gcd1JniRootsProbe_foreignDetached(JNIEnv *env, jclass self)
{
    (void)env;
    (void)self;
    return ATOMIC_LOAD_ACQUIRE(&g_detached) ? JNI_TRUE : JNI_FALSE;
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
