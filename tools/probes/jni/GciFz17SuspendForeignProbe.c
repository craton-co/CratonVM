/*
 * gci wave i17, lane fz: the agent and natives of
 * tools/probes/GciFz17SuspendForeignProbe.java (JVMTI SuspendThread of a host
 * thread attached with AttachCurrentThreadAsDaemon that loops over JNIEnv
 * calls from its own C code). Loaded twice: as -agentpath:<lib>
 * (Agent_OnLoad takes the jvmtiEnv and adds can_suspend) and with
 * System.load(<lib>) for the natives.
 *
 *   Windows: cl /LD /O1 /I"%JDK%\include" /I"%JDK%\include\win32" GciFz17SuspendForeignProbe.c /Fegcifz17sus.dll
 *   Linux:   gcc -shared -fPIC -O1 -I$JDK/include -I$JDK/include/linux -o libgcifz17sus.so GciFz17SuspendForeignProbe.c -lpthread
 */
#include <string.h>
#include <jni.h>
#include <jvmti.h>
#ifdef _WIN32
#include <windows.h>
#else
#include <pthread.h>
#endif

static jvmtiEnv *jvmti;
static jint caps_err = -1;
static JavaVM *jvm;

/* The looper's state. One looper at a time (`start` refuses a second). */
static volatile jint stop_flag;
static volatile jint done_flag = 1;
static volatile jint looper_rc;
static volatile jlong calls;
static volatile jint looper_kind;
static jobject volatile looper_thread; /* global ref to the looper's Thread */
static jobject volatile looper_arr;    /* global ref to the int[4] */

JNIEXPORT jint JNICALL Agent_OnLoad(JavaVM *vm, char *options, void *reserved) {
    jvmtiCapabilities caps;
    (void)options;
    (void)reserved;
    if ((*vm)->GetEnv(vm, (void **)&jvmti, JVMTI_VERSION_1_2) != JNI_OK) {
        jvmti = NULL;
        return JNI_ERR;
    }
    memset(&caps, 0, sizeof(caps));
    caps.can_suspend = 1;
    caps_err = (jint)(*jvmti)->AddCapabilities(jvmti, &caps);
    return JNI_OK;
}

/* 0 with the agent's jvmtiEnv and its capability; else -1 or -error. */
JNIEXPORT jint JNICALL Java_GciFz17SuspendForeignProbe_init(JNIEnv *env, jclass cls) {
    (void)cls;
    if (jvmti == NULL) {
        return -1;
    }
    if ((*env)->GetJavaVM(env, &jvm) != JNI_OK) {
        return -2;
    }
    return caps_err == 0 ? 0 : -caps_err;
}

/*
 * The host thread: attach as a daemon, publish its own java.lang.Thread,
 * then until stop_flag one JNIEnv call per round, then calls++.
 *   kind 0: NewStringUTF + DeleteLocalRef (full JNIEnv entries),
 *   kind 1: GetArrayLength on the global int[] (a leaf JNIEnv function).
 * looper_rc: 0, or a negative code naming the first failing call.
 */
static void looper_body(void) {
    JNIEnv *env = NULL;
    JavaVMAttachArgs args;
    jclass tc;
    jmethodID cur;
    jobject self;
    jobject t;
    args.version = JNI_VERSION_1_8;
    args.name = (char *)"fz17-foreign";
    args.group = NULL;
    if ((*jvm)->AttachCurrentThreadAsDaemon(jvm, (void **)&env, &args) != JNI_OK) {
        looper_rc = -10;
        done_flag = 1;
        return;
    }
    tc = (*env)->FindClass(env, "java/lang/Thread");
    cur = tc == NULL ? NULL : (*env)->GetStaticMethodID(env, tc, "currentThread", "()Ljava/lang/Thread;");
    self = cur == NULL ? NULL : (*env)->CallStaticObjectMethod(env, tc, cur);
    if (self == NULL) {
        looper_rc = -11;
        (*jvm)->DetachCurrentThread(jvm);
        done_flag = 1;
        return;
    }
    looper_thread = (*env)->NewGlobalRef(env, self);
    (*env)->DeleteLocalRef(env, self);
    (*env)->DeleteLocalRef(env, tc);
    while (!stop_flag) {
        if (looper_kind == 0) {
            jstring s = (*env)->NewStringUTF(env, "fz17");
            if (s == NULL) {
                looper_rc = -1;
                break;
            }
            (*env)->DeleteLocalRef(env, s);
        } else {
            if ((*env)->GetArrayLength(env, (jarray)looper_arr) != 4) {
                looper_rc = -2;
                break;
            }
        }
        calls++;
    }
    t = looper_thread;
    looper_thread = NULL;
    (*env)->DeleteGlobalRef(env, t);
    (*jvm)->DetachCurrentThread(jvm);
    done_flag = 1;
}

#ifdef _WIN32
static DWORD WINAPI looper_main(LPVOID p) {
    (void)p;
    looper_body();
    return 0;
}
#else
static void *looper_main(void *p) {
    (void)p;
    looper_body();
    return NULL;
}
#endif

/* Start the looper of `kind` on `arr`: 0, -3 (one still runs) or -4 (no thread). */
JNIEXPORT jint JNICALL Java_GciFz17SuspendForeignProbe_start(JNIEnv *env, jclass cls, jint kind,
                                                             jintArray arr) {
    (void)cls;
    if (!done_flag) {
        return -3;
    }
    if (looper_arr != NULL) {
        (*env)->DeleteGlobalRef(env, looper_arr);
        looper_arr = NULL;
    }
    looper_arr = (*env)->NewGlobalRef(env, arr);
    looper_kind = kind;
    looper_rc = 0;
    looper_thread = NULL;
    stop_flag = 0;
    done_flag = 0;
#ifdef _WIN32
    {
        HANDLE h = CreateThread(NULL, 0, looper_main, NULL, 0, NULL);
        if (h == NULL) {
            done_flag = 1;
            return -4;
        }
        CloseHandle(h);
    }
#else
    {
        pthread_t th;
        if (pthread_create(&th, NULL, looper_main, NULL) != 0) {
            done_flag = 1;
            return -4;
        }
        pthread_detach(th);
    }
#endif
    return 0;
}

/* The looper's Thread (a new local ref) once it attached, else NULL. */
JNIEXPORT jobject JNICALL Java_GciFz17SuspendForeignProbe_looperThread(JNIEnv *env, jclass cls) {
    jobject t = looper_thread;
    (void)cls;
    return t == NULL ? NULL : (*env)->NewLocalRef(env, t);
}

JNIEXPORT void JNICALL Java_GciFz17SuspendForeignProbe_setStop(JNIEnv *env, jclass cls, jint v) {
    (void)env;
    (void)cls;
    stop_flag = v;
}

JNIEXPORT jint JNICALL Java_GciFz17SuspendForeignProbe_done(JNIEnv *env, jclass cls) {
    (void)env;
    (void)cls;
    return done_flag;
}

JNIEXPORT jint JNICALL Java_GciFz17SuspendForeignProbe_rc(JNIEnv *env, jclass cls) {
    (void)env;
    (void)cls;
    return looper_rc;
}

JNIEXPORT jlong JNICALL Java_GciFz17SuspendForeignProbe_calls(JNIEnv *env, jclass cls) {
    (void)env;
    (void)cls;
    return calls;
}

JNIEXPORT jint JNICALL Java_GciFz17SuspendForeignProbe_suspend(JNIEnv *env, jclass cls, jthread t) {
    (void)env;
    (void)cls;
    return (jint)(*jvmti)->SuspendThread(jvmti, t);
}

JNIEXPORT jint JNICALL Java_GciFz17SuspendForeignProbe_resume(JNIEnv *env, jclass cls, jthread t) {
    (void)env;
    (void)cls;
    return (jint)(*jvmti)->ResumeThread(jvmti, t);
}

/* The JVMTI thread state of t, masked to SUSPENDED | IN_NATIVE | ALIVE. */
JNIEXPORT jint JNICALL Java_GciFz17SuspendForeignProbe_state(JNIEnv *env, jclass cls, jthread t) {
    jint state = 0;
    (void)env;
    (void)cls;
    if ((*jvmti)->GetThreadState(jvmti, t, &state) != JVMTI_ERROR_NONE) {
        return -1;
    }
    return state & (JVMTI_THREAD_STATE_SUSPENDED | JVMTI_THREAD_STATE_IN_NATIVE | JVMTI_THREAD_STATE_ALIVE);
}
