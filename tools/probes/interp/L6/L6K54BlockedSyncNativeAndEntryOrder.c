/*
 * Native shim of tools/probes/interp/L6/L6K54BlockedSyncNativeAndEntryOrder.java:
 * the JVMTI agent (MethodEntry on one thread, GetStackTrace of another) and
 * the two `synchronized native` methods, which do nothing.
 */
#include <jni.h>
#include <jvmti.h>
#include <stdio.h>
#include <string.h>

static jvmtiEnv *agent;
static char entry_line[256];
static int entries;

static void JNICALL on_entry(jvmtiEnv *jvmti, JNIEnv *env, jthread thread, jmethodID method) {
    char *name = NULL;
    jint count = -1;
    jobject *monitors = NULL;
    if ((*jvmti)->GetMethodName(jvmti, method, &name, NULL, NULL) != JVMTI_ERROR_NONE || name == NULL) {
        return;
    }
    if (strcmp(name, "contended") == 0) {
        entries++;
    }
    if (strcmp(name, "contended") == 0 && entry_line[0] == '\0') {
        jvmtiError err = (*jvmti)->GetOwnedMonitorInfo(jvmti, thread, &count, &monitors);
        snprintf(entry_line, sizeof entry_line, "contended err=%d owned=%d", (int)err, (int)count);
        if (monitors != NULL) {
            (*jvmti)->Deallocate(jvmti, (unsigned char *)monitors);
        }
    }
    (*jvmti)->Deallocate(jvmti, (unsigned char *)name);
}

JNIEXPORT jint JNICALL Agent_OnLoad(JavaVM *vm, char *options, void *reserved) {
    jvmtiCapabilities caps;
    jvmtiEventCallbacks callbacks;
    if ((*vm)->GetEnv(vm, (void **)&agent, JVMTI_VERSION_1_2) != JNI_OK) {
        return 1;
    }
    memset(&caps, 0, sizeof caps);
    caps.can_generate_method_entry_events = 1;
    caps.can_get_owned_monitor_info = 1;
    (*agent)->AddCapabilities(agent, &caps);
    memset(&callbacks, 0, sizeof callbacks);
    callbacks.MethodEntry = on_entry;
    (*agent)->SetEventCallbacks(agent, &callbacks, sizeof callbacks);
    return 0;
}

JNIEXPORT void JNICALL Java_L6K54BlockedSyncNativeAndEntryOrder_arm(JNIEnv *env, jclass cls, jthread t) {
    if (agent == NULL) {
        return;
    }
    entry_line[0] = '\0';
    entries = 0;
    (*agent)->SetEventNotificationMode(agent, JVMTI_ENABLE, JVMTI_EVENT_METHOD_ENTRY, t);
}

JNIEXPORT jstring JNICALL Java_L6K54BlockedSyncNativeAndEntryOrder_disarm(JNIEnv *env, jclass cls, jthread t) {
    if (agent == NULL) {
        return (*env)->NewStringUTF(env, "no JVMTI env");
    }
    char out[300];
    (*agent)->SetEventNotificationMode(agent, JVMTI_DISABLE, JVMTI_EVENT_METHOD_ENTRY, t);
    if (entry_line[0] == '\0') {
        return (*env)->NewStringUTF(env, "none");
    }
    snprintf(out, sizeof out, "%s entries=%d", entry_line, entries);
    return (*env)->NewStringUTF(env, out);
}

JNIEXPORT jstring JNICALL Java_L6K54BlockedSyncNativeAndEntryOrder_top(JNIEnv *env, jclass cls, jthread t) {
    jvmtiFrameInfo frames[2];
    jint count = 0;
    char out[256];
    size_t at = 0;
    jvmtiError err;
    int i;
    if (agent == NULL) {
        return (*env)->NewStringUTF(env, "no JVMTI env");
    }
    err = (*agent)->GetStackTrace(agent, t, 0, 2, frames, &count);
    at += snprintf(out + at, sizeof out - at, "err=%d", (int)err);
    for (i = 0; err == JVMTI_ERROR_NONE && i < count; i++) {
        char *name = NULL;
        (*agent)->GetMethodName(agent, frames[i].method, &name, NULL, NULL);
        at += snprintf(out + at, sizeof out - at, "%s%s@%lld", i == 0 ? " " : " < ", name ? name : "?",
                       (long long)frames[i].location);
        if (name != NULL) {
            (*agent)->Deallocate(agent, (unsigned char *)name);
        }
    }
    return (*env)->NewStringUTF(env, out);
}

JNIEXPORT void JNICALL Java_L6K54BlockedSyncNativeAndEntryOrder_staticNative(JNIEnv *env, jclass cls) {
}

JNIEXPORT void JNICALL Java_L6K54BlockedSyncNativeAndEntryOrder_instanceNative(JNIEnv *env, jobject self) {
}
