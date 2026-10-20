/*
 * gci wave i11, lane ja: the agent and natives of
 * tools/probes/GciJa11EventCallbackGcSafeProbe.java (GC-safe JVMTI calls --
 * RawMonitorWait, ForceGarbageCollection -- from MethodEntry / FramePop
 * callbacks while collections are requested back to back). Loaded twice: as
 * -agentpath:<lib> (Agent_OnLoad takes the jvmtiEnv, its capabilities and
 * callbacks) and with System.load(<lib>) for the natives.
 *
 *   Windows: cl /LD /O1 /I"%JDK%\include" /I"%JDK%\include\win32" GciJa11EventCallbackGcSafeProbe.c /Fegcija11cb.dll
 *   Linux:   gcc -shared -fPIC -O1 -I$JDK/include -I$JDK/include/linux -o libgcija11cb.so GciJa11EventCallbackGcSafeProbe.c
 */
#include <jni.h>
#include <jvmti.h>
#include <string.h>

static jvmtiEnv *jvmti;
static jrawMonitorID mon;
static jmethodID hook_mid;
static int have_frame_pop;

/* 0 hook entries, 1 raw monitor errors, 2 ForceGarbageCollection errors,
 * 3 hook FramePops, 4 NotifyFramePop errors, 5 FramePop capability,
 * 6 hook MethodExits. */
static volatile jlong ja_stat[7];

/* Enter the raw monitor and wait on it for 1 ms: a GC-safe wait. */
static void wait_a_little(jvmtiEnv *jv) {
    if ((*jv)->RawMonitorEnter(jv, mon) != JVMTI_ERROR_NONE) {
        ja_stat[1]++;
        return;
    }
    if ((*jv)->RawMonitorWait(jv, mon, 1) != JVMTI_ERROR_NONE) {
        ja_stat[1]++;
    }
    if ((*jv)->RawMonitorExit(jv, mon) != JVMTI_ERROR_NONE) {
        ja_stat[1]++;
    }
}

static void JNICALL on_method_entry(jvmtiEnv *jv, JNIEnv *env, jthread thread, jmethodID method) {
    jlong n;
    (void)env;
    if (hook_mid == NULL || method != hook_mid) {
        return;
    }
    n = ++ja_stat[0];
    wait_a_little(jv);
    if (n % 20 == 0 && (*jv)->ForceGarbageCollection(jv) != JVMTI_ERROR_NONE) {
        ja_stat[2]++;
    }
    if (have_frame_pop && (*jv)->NotifyFramePop(jv, thread, 0) != JVMTI_ERROR_NONE) {
        ja_stat[4]++;
    }
}

static void JNICALL on_frame_pop(jvmtiEnv *jv, JNIEnv *env, jthread thread, jmethodID method,
                                 jboolean was_popped_by_exception) {
    (void)env;
    (void)thread;
    (void)was_popped_by_exception;
    if (hook_mid == NULL || method != hook_mid) {
        return;
    }
    ja_stat[3]++;
    wait_a_little(jv);
}

static void JNICALL on_method_exit(jvmtiEnv *jv, JNIEnv *env, jthread thread, jmethodID method,
                                   jboolean was_popped_by_exception, jvalue return_value) {
    (void)env;
    (void)thread;
    (void)was_popped_by_exception;
    (void)return_value;
    if (hook_mid == NULL || method != hook_mid) {
        return;
    }
    ja_stat[6]++;
    wait_a_little(jv);
}

JNIEXPORT jint JNICALL Agent_OnLoad(JavaVM *vm, char *options, void *reserved) {
    jvmtiCapabilities caps;
    jvmtiEventCallbacks callbacks;
    (void)options;
    (void)reserved;
    if ((*vm)->GetEnv(vm, (void **)&jvmti, JVMTI_VERSION_1_2) != JNI_OK) {
        return JNI_ERR;
    }
    memset(&caps, 0, sizeof(caps));
    caps.can_generate_method_entry_events = 1;
    caps.can_generate_method_exit_events = 1;
    caps.can_generate_frame_pop_events = 1;
    if ((*jvmti)->AddCapabilities(jvmti, &caps) == JVMTI_ERROR_NONE) {
        have_frame_pop = 1;
    } else {
        memset(&caps, 0, sizeof(caps));
        caps.can_generate_method_entry_events = 1;
        caps.can_generate_method_exit_events = 1;
        if ((*jvmti)->AddCapabilities(jvmti, &caps) != JVMTI_ERROR_NONE) {
            return JNI_ERR;
        }
    }
    ja_stat[5] = have_frame_pop;
    if ((*jvmti)->CreateRawMonitor(jvmti, "ja11", &mon) != JVMTI_ERROR_NONE) {
        return JNI_ERR;
    }
    memset(&callbacks, 0, sizeof(callbacks));
    callbacks.MethodEntry = on_method_entry;
    callbacks.FramePop = on_frame_pop;
    callbacks.MethodExit = on_method_exit;
    if ((*jvmti)->SetEventCallbacks(jvmti, &callbacks, (jint)sizeof(callbacks)) != JVMTI_ERROR_NONE) {
        return JNI_ERR;
    }
    return JNI_OK;
}

JNIEXPORT jint JNICALL Java_GciJa11EventCallbackGcSafeProbe_init(JNIEnv *env, jclass cls, jclass self) {
    (void)cls;
    if (jvmti == NULL) {
        return -1;
    }
    hook_mid = (*env)->GetStaticMethodID(env, self, "hook", "(I)I");
    return hook_mid == NULL ? -2 : 0;
}

JNIEXPORT jint JNICALL Java_GciJa11EventCallbackGcSafeProbe_arm(JNIEnv *env, jclass cls, jthread t) {
    jvmtiError err;
    (void)env;
    (void)cls;
    err = (*jvmti)->SetEventNotificationMode(jvmti, JVMTI_ENABLE, JVMTI_EVENT_METHOD_ENTRY, t);
    if (err == JVMTI_ERROR_NONE) {
        err = (*jvmti)->SetEventNotificationMode(jvmti, JVMTI_ENABLE, JVMTI_EVENT_METHOD_EXIT, t);
    }
    if (err == JVMTI_ERROR_NONE && have_frame_pop) {
        err = (*jvmti)->SetEventNotificationMode(jvmti, JVMTI_ENABLE, JVMTI_EVENT_FRAME_POP, t);
    }
    return (jint)err;
}

JNIEXPORT jint JNICALL Java_GciJa11EventCallbackGcSafeProbe_disarm(JNIEnv *env, jclass cls, jthread t) {
    jvmtiError err;
    (void)env;
    (void)cls;
    err = (*jvmti)->SetEventNotificationMode(jvmti, JVMTI_DISABLE, JVMTI_EVENT_METHOD_ENTRY, t);
    if (err == JVMTI_ERROR_NONE) {
        err = (*jvmti)->SetEventNotificationMode(jvmti, JVMTI_DISABLE, JVMTI_EVENT_METHOD_EXIT, t);
    }
    if (err == JVMTI_ERROR_NONE && have_frame_pop) {
        err = (*jvmti)->SetEventNotificationMode(jvmti, JVMTI_DISABLE, JVMTI_EVENT_FRAME_POP, t);
    }
    return (jint)err;
}

JNIEXPORT jlong JNICALL Java_GciJa11EventCallbackGcSafeProbe_stat(JNIEnv *env, jclass cls, jint which) {
    (void)env;
    (void)cls;
    return which >= 0 && which < 7 ? ja_stat[which] : -1;
}
