/*
 * gci wave i12, lane ev: the agent and natives of
 * tools/probes/GciEv12ThreadClassEventProbe.java (GC-safe JVMTI calls --
 * RawMonitorWait, ForceGarbageCollection -- from ThreadStart / ThreadEnd /
 * ClassLoad / ClassPrepare callbacks while collections are requested back
 * to back). Loaded twice: as -agentpath:<lib> (Agent_OnLoad takes the
 * jvmtiEnv and its callbacks) and with System.load(<lib>) for the natives.
 *
 *   Windows: cl /LD /O1 /I"%JDK%\include" /I"%JDK%\include\win32" GciEv12ThreadClassEventProbe.c /Fegciev12tc.dll
 *   Linux:   gcc -shared -fPIC -O1 -I$JDK/include -I$JDK/include/linux -o libgciev12tc.so GciEv12ThreadClassEventProbe.c
 */
#include <jni.h>
#include <jvmti.h>
#include <string.h>

static jvmtiEnv *jvmti;
static jrawMonitorID mon;

/* 0 worker ThreadStarts, 1 worker ThreadEnds, 2 GciEv12G* ClassLoads,
 * 3 GciEv12G* ClassPrepares, 4 raw monitor errors, 5 ForceGarbageCollection
 * errors, 6 GetThreadInfo errors, 7 GetClassSignature errors, 8 the probe's
 * lambda ClassLoads, 9 the probe's lambda ClassPrepares, 10 ClassLoads of
 * `java.util.concurrent.ConcurrentSkipList*` (a bootstrap load: the VM's
 * thread-less load path). Written under `mon` (an error of RawMonitorEnter
 * itself is counted without it). */
#define EV_STATS 11
static volatile jlong ev_stat[EV_STATS];

/* Count `which` and wait on the raw monitor for 1 ms (a GC-safe wait).
 * Answers the new count, 0 when the monitor could not be entered. */
static jlong bump(jvmtiEnv *jv, int which) {
    jlong n;
    if ((*jv)->RawMonitorEnter(jv, mon) != JVMTI_ERROR_NONE) {
        ev_stat[4]++;
        return 0;
    }
    n = ++ev_stat[which];
    if ((*jv)->RawMonitorWait(jv, mon, 1) != JVMTI_ERROR_NONE) {
        ev_stat[4]++;
    }
    if ((*jv)->RawMonitorExit(jv, mon) != JVMTI_ERROR_NONE) {
        ev_stat[4]++;
    }
    return n;
}

/* Is `thread` one of the probe's workers (named `ev12-w<n>`)? */
static int is_worker(jvmtiEnv *jv, jthread thread) {
    jvmtiThreadInfo info;
    int hit;
    memset(&info, 0, sizeof(info));
    if ((*jv)->GetThreadInfo(jv, thread, &info) != JVMTI_ERROR_NONE) {
        bump(jv, 6);
        return 0;
    }
    hit = info.name != NULL && strncmp(info.name, "ev12-w", 6) == 0;
    if (info.name != NULL) {
        (*jv)->Deallocate(jv, (unsigned char *)info.name);
    }
    return hit;
}

static void thread_event(jvmtiEnv *jv, jthread thread, int which) {
    jlong n;
    if (!is_worker(jv, thread)) {
        return;
    }
    n = bump(jv, which);
    if (n % 8 == 0 && (*jv)->ForceGarbageCollection(jv) != JVMTI_ERROR_NONE) {
        bump(jv, 5);
    }
}

static void JNICALL on_thread_start(jvmtiEnv *jv, JNIEnv *env, jthread thread) {
    (void)env;
    thread_event(jv, thread, 0);
}

static void JNICALL on_thread_end(jvmtiEnv *jv, JNIEnv *env, jthread thread) {
    (void)env;
    thread_event(jv, thread, 1);
}

/* `which` is 2 (ClassLoad) or 3 (ClassPrepare); the probe's lambda proxy
 * classes count at `which + 6`. */
static void class_event(jvmtiEnv *jv, jclass klass, int which) {
    char *sig = NULL;
    int hit = 0;
    int lambda = 0;
    int jdk = 0;
    if ((*jv)->GetClassSignature(jv, klass, &sig, NULL) != JVMTI_ERROR_NONE) {
        bump(jv, 7);
        return;
    }
    if (sig != NULL) {
        hit = strncmp(sig, "LGciEv12G", 9) == 0;
        lambda = strstr(sig, "GciEv12ThreadClassEventProbe$$Lambda") != NULL;
        jdk = strncmp(sig, "Ljava/util/concurrent/ConcurrentSkipList", 40) == 0;
        (*jv)->Deallocate(jv, (unsigned char *)sig);
    }
    if (hit) {
        bump(jv, which);
    } else if (lambda) {
        bump(jv, which + 6);
    } else if (jdk && which == 2) {
        bump(jv, 10);
    }
}

static void JNICALL on_class_load(jvmtiEnv *jv, JNIEnv *env, jthread thread, jclass klass) {
    (void)env;
    (void)thread;
    class_event(jv, klass, 2);
}

static void JNICALL on_class_prepare(jvmtiEnv *jv, JNIEnv *env, jthread thread, jclass klass) {
    (void)env;
    (void)thread;
    class_event(jv, klass, 3);
}

JNIEXPORT jint JNICALL Agent_OnLoad(JavaVM *vm, char *options, void *reserved) {
    jvmtiEventCallbacks callbacks;
    (void)options;
    (void)reserved;
    if ((*vm)->GetEnv(vm, (void **)&jvmti, JVMTI_VERSION_1_2) != JNI_OK) {
        return JNI_ERR;
    }
    if ((*jvmti)->CreateRawMonitor(jvmti, "ev12", &mon) != JVMTI_ERROR_NONE) {
        return JNI_ERR;
    }
    memset(&callbacks, 0, sizeof(callbacks));
    callbacks.ThreadStart = on_thread_start;
    callbacks.ThreadEnd = on_thread_end;
    callbacks.ClassLoad = on_class_load;
    callbacks.ClassPrepare = on_class_prepare;
    if ((*jvmti)->SetEventCallbacks(jvmti, &callbacks, (jint)sizeof(callbacks)) != JVMTI_ERROR_NONE) {
        return JNI_ERR;
    }
    return JNI_OK;
}

static const jvmtiEvent ev_events[4] = {
    JVMTI_EVENT_THREAD_START, JVMTI_EVENT_THREAD_END, JVMTI_EVENT_CLASS_LOAD, JVMTI_EVENT_CLASS_PREPARE
};

static jint set_all(jvmtiEventMode mode) {
    int i;
    jvmtiError err = JVMTI_ERROR_NONE;
    for (i = 0; i < 4 && err == JVMTI_ERROR_NONE; i++) {
        err = (*jvmti)->SetEventNotificationMode(jvmti, mode, ev_events[i], NULL);
    }
    return (jint)err;
}

JNIEXPORT jint JNICALL Java_GciEv12ThreadClassEventProbe_init(JNIEnv *env, jclass cls) {
    (void)env;
    (void)cls;
    return jvmti == NULL ? -1 : 0;
}

JNIEXPORT jint JNICALL Java_GciEv12ThreadClassEventProbe_arm(JNIEnv *env, jclass cls) {
    (void)env;
    (void)cls;
    return set_all(JVMTI_ENABLE);
}

JNIEXPORT jint JNICALL Java_GciEv12ThreadClassEventProbe_disarm(JNIEnv *env, jclass cls) {
    (void)env;
    (void)cls;
    return set_all(JVMTI_DISABLE);
}

JNIEXPORT jlong JNICALL Java_GciEv12ThreadClassEventProbe_stat(JNIEnv *env, jclass cls, jint which) {
    (void)env;
    (void)cls;
    return which >= 0 && which < EV_STATS ? ev_stat[which] : -1;
}
