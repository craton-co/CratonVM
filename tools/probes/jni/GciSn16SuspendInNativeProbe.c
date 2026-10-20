/*
 * gci wave i16, lane sn: the agent and natives of
 * tools/probes/GciSn16SuspendInNativeProbe.java (JVMTI SuspendThread of a
 * thread whose JNI native's C code loops over JNIEnv / JVMTI calls). Loaded
 * twice: as -agentpath:<lib> (Agent_OnLoad takes the jvmtiEnv and adds
 * can_suspend and can_tag_objects) and with System.load(<lib>) for the
 * natives.
 *
 *   Windows: cl /LD /O1 /I"%JDK%\include" /I"%JDK%\include\win32" GciSn16SuspendInNativeProbe.c /Fegcisn16sus.dll
 *   Linux:   gcc -shared -fPIC -O1 -I$JDK/include -I$JDK/include/linux -o libgcisn16sus.so GciSn16SuspendInNativeProbe.c
 */
#include <string.h>
#include <jni.h>
#include <jvmti.h>

static jvmtiEnv *jvmti;
static jint caps_err = -1;

/* The looper's state: the stop request and the calls it completed. */
static volatile jint stop_flag;
static volatile jlong calls;

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
    caps.can_tag_objects = 1;
    caps_err = (jint)(*jvmti)->AddCapabilities(jvmti, &caps);
    return JNI_OK;
}

/* 0 with the agent's jvmtiEnv and both capabilities; else -1 or -error. */
JNIEXPORT jint JNICALL Java_GciSn16SuspendInNativeProbe_init(JNIEnv *env, jclass cls) {
    (void)env;
    (void)cls;
    if (jvmti == NULL) {
        return -1;
    }
    return caps_err == 0 ? 0 : -caps_err;
}

/*
 * Loop until stop_flag: one VM entry per round, then calls++.
 *   kind 0: GetObjectClass + DeleteLocalRef (a full JNIEnv entry),
 *   kind 1: GetArrayLength (a leaf JNIEnv function),
 *   kind 2: JVMTI GetTag (a C JVMTI function that decodes a handle),
 *   kind 3: JVMTI GetThreadState of the calling thread (a C JVMTI thread
 *           function, no handle).
 * Returns 0, or a negative code naming the first failing call.
 */
JNIEXPORT jint JNICALL Java_GciSn16SuspendInNativeProbe_loop(JNIEnv *env, jclass cls, jint kind,
                                                              jobject obj, jintArray arr) {
    (void)cls;
    while (!stop_flag) {
        if (kind == 0) {
            jclass c = (*env)->GetObjectClass(env, obj);
            if (c == NULL) {
                return -1;
            }
            (*env)->DeleteLocalRef(env, c);
        } else if (kind == 1) {
            if ((*env)->GetArrayLength(env, arr) != 4) {
                return -2;
            }
        } else if (kind == 2) {
            jlong tag = 0;
            if ((*jvmti)->GetTag(jvmti, obj, &tag) != JVMTI_ERROR_NONE) {
                return -3;
            }
        } else {
            jint state = 0;
            if ((*jvmti)->GetThreadState(jvmti, NULL, &state) != JVMTI_ERROR_NONE) {
                return -4;
            }
        }
        calls++;
    }
    return 0;
}

JNIEXPORT void JNICALL Java_GciSn16SuspendInNativeProbe_setStop(JNIEnv *env, jclass cls, jint v) {
    (void)env;
    (void)cls;
    stop_flag = v;
}

JNIEXPORT jlong JNICALL Java_GciSn16SuspendInNativeProbe_calls(JNIEnv *env, jclass cls) {
    (void)env;
    (void)cls;
    return calls;
}

JNIEXPORT jint JNICALL Java_GciSn16SuspendInNativeProbe_suspend(JNIEnv *env, jclass cls, jthread t) {
    (void)env;
    (void)cls;
    return (jint)(*jvmti)->SuspendThread(jvmti, t);
}

JNIEXPORT jint JNICALL Java_GciSn16SuspendInNativeProbe_resume(JNIEnv *env, jclass cls, jthread t) {
    (void)env;
    (void)cls;
    return (jint)(*jvmti)->ResumeThread(jvmti, t);
}

/* The JVMTI thread state of t, masked to SUSPENDED | IN_NATIVE | ALIVE. */
JNIEXPORT jint JNICALL Java_GciSn16SuspendInNativeProbe_state(JNIEnv *env, jclass cls, jthread t) {
    jint state = 0;
    (void)env;
    (void)cls;
    if ((*jvmti)->GetThreadState(jvmti, t, &state) != JVMTI_ERROR_NONE) {
        return -1;
    }
    return state & (JVMTI_THREAD_STATE_SUSPENDED | JVMTI_THREAD_STATE_IN_NATIVE | JVMTI_THREAD_STATE_ALIVE);
}
