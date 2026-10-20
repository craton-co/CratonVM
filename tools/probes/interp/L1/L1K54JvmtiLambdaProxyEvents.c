/*
 * Native shim of tools/probes/interp/L1/L1K54JvmtiLambdaProxyEvents.java:
 * the class and method events of lambdas' classes between start and stop.
 */
#include <jni.h>
#include <jvmti.h>
#include <stdio.h>
#include <string.h>

static jvmtiEnv *agent;
static volatile int recording;
static char lines[8192];

static void mask(const char *in, char *out, size_t cap) {
    size_t at = 0;
    while (*in != '\0' && at + 4 < cap) {
        if (in[0] == '0' && in[1] == 'x') {
            out[at++] = '0';
            out[at++] = 'x';
            out[at++] = '?';
            in += 2;
            while ((*in >= '0' && *in <= '9') || (*in >= 'a' && *in <= 'f')) {
                in++;
            }
        } else {
            out[at++] = *in++;
        }
    }
    out[at] = '\0';
}

static void note(const char *what, const char *name) {
    char masked[512];
    size_t used = strlen(lines);
    mask(name, masked, sizeof masked);
    snprintf(lines + used, sizeof lines - used, "%s %s\n", what, masked);
}

static void class_event(jvmtiEnv *jvmti, const char *what, jclass klass) {
    char *sig = NULL;
    if (!recording || (*jvmti)->GetClassSignature(jvmti, klass, &sig, NULL) != JVMTI_ERROR_NONE) {
        return;
    }
    if (strstr(sig, "$$Lambda") != NULL) {
        note(what, sig);
    }
    (*jvmti)->Deallocate(jvmti, (unsigned char *)sig);
}

static void method_event(jvmtiEnv *jvmti, const char *what, jmethodID method) {
    jclass declaring = NULL;
    char *sig = NULL;
    char *name = NULL;
    if (!recording || (*jvmti)->GetMethodDeclaringClass(jvmti, method, &declaring) != JVMTI_ERROR_NONE
        || (*jvmti)->GetClassSignature(jvmti, declaring, &sig, NULL) != JVMTI_ERROR_NONE) {
        return;
    }
    if (strstr(sig, "$$Lambda") != NULL && (*jvmti)->GetMethodName(jvmti, method, &name, NULL, NULL) == JVMTI_ERROR_NONE) {
        char both[512];
        snprintf(both, sizeof both, "%s.%s", sig, name);
        note(what, both);
        (*jvmti)->Deallocate(jvmti, (unsigned char *)name);
    }
    (*jvmti)->Deallocate(jvmti, (unsigned char *)sig);
}

static void JNICALL on_hook(jvmtiEnv *jvmti, JNIEnv *env, jclass redefined, jobject loader, const char *name,
                            jobject pd, jint len, const unsigned char *data, jint *new_len, unsigned char **new_data) {
    if (recording && name != NULL && strstr(name, "$$Lambda") != NULL) {
        note("hook", name);
    }
}

static void JNICALL on_load(jvmtiEnv *jvmti, JNIEnv *env, jthread thread, jclass klass) {
    class_event(jvmti, "load", klass);
}

static void JNICALL on_prepare(jvmtiEnv *jvmti, JNIEnv *env, jthread thread, jclass klass) {
    class_event(jvmti, "prepare", klass);
}

static void JNICALL on_entry(jvmtiEnv *jvmti, JNIEnv *env, jthread thread, jmethodID method) {
    method_event(jvmti, "entry", method);
}

static void JNICALL on_exit(jvmtiEnv *jvmti, JNIEnv *env, jthread thread, jmethodID method, jboolean popped,
                            jvalue value) {
    method_event(jvmti, "exit", method);
}

JNIEXPORT jint JNICALL Agent_OnLoad(JavaVM *vm, char *options, void *reserved) {
    jvmtiCapabilities caps;
    jvmtiEventCallbacks callbacks;
    if ((*vm)->GetEnv(vm, (void **)&agent, JVMTI_VERSION_1_2) != JNI_OK) {
        return 1;
    }
    memset(&caps, 0, sizeof caps);
    caps.can_generate_method_entry_events = 1;
    caps.can_generate_method_exit_events = 1;
    (*agent)->AddCapabilities(agent, &caps);
    memset(&callbacks, 0, sizeof callbacks);
    callbacks.ClassFileLoadHook = on_hook;
    callbacks.ClassLoad = on_load;
    callbacks.ClassPrepare = on_prepare;
    callbacks.MethodEntry = on_entry;
    callbacks.MethodExit = on_exit;
    (*agent)->SetEventCallbacks(agent, &callbacks, sizeof callbacks);
    (*agent)->SetEventNotificationMode(agent, JVMTI_ENABLE, JVMTI_EVENT_CLASS_FILE_LOAD_HOOK, NULL);
    (*agent)->SetEventNotificationMode(agent, JVMTI_ENABLE, JVMTI_EVENT_CLASS_LOAD, NULL);
    (*agent)->SetEventNotificationMode(agent, JVMTI_ENABLE, JVMTI_EVENT_CLASS_PREPARE, NULL);
    return 0;
}

JNIEXPORT void JNICALL Java_L1K54JvmtiLambdaProxyEvents_start(JNIEnv *env, jclass cls) {
    jthread thread = NULL;
    if (agent == NULL) {
        return;
    }
    lines[0] = '\0';
    (*agent)->GetCurrentThread(agent, &thread);
    (*agent)->SetEventNotificationMode(agent, JVMTI_ENABLE, JVMTI_EVENT_METHOD_ENTRY, thread);
    (*agent)->SetEventNotificationMode(agent, JVMTI_ENABLE, JVMTI_EVENT_METHOD_EXIT, thread);
    recording = 1;
}

JNIEXPORT jstring JNICALL Java_L1K54JvmtiLambdaProxyEvents_stop(JNIEnv *env, jclass cls) {
    jthread thread = NULL;
    if (agent == NULL) {
        return (*env)->NewStringUTF(env, "no JVMTI env\n");
    }
    recording = 0;
    (*agent)->GetCurrentThread(agent, &thread);
    (*agent)->SetEventNotificationMode(agent, JVMTI_DISABLE, JVMTI_EVENT_METHOD_ENTRY, thread);
    (*agent)->SetEventNotificationMode(agent, JVMTI_DISABLE, JVMTI_EVENT_METHOD_EXIT, thread);
    return (*env)->NewStringUTF(env, lines);
}
