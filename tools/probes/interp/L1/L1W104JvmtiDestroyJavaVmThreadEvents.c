/*
 * Agent for tools/probes/interp/L1/L1W104JvmtiDestroyJavaVmThreadEvents.java
 * (interpreter round i5 wave 104, lane L1): prints the ThreadStart and
 * ThreadEnd events of the threads named main, DestroyJavaVM and lingerer.
 * Build instructions are in the probe's header.
 */
#include <jni.h>
#include <jvmti.h>
#include <stdio.h>
#include <string.h>

static void
report(jvmtiEnv *jvmti, jthread thread, const char *what)
{
    jvmtiThreadInfo info;
    memset(&info, 0, sizeof info);
    if ((*jvmti)->GetThreadInfo(jvmti, thread, &info) != JVMTI_ERROR_NONE || info.name == NULL) {
        return;
    }
    if (strcmp(info.name, "main") == 0 || strcmp(info.name, "DestroyJavaVM") == 0
        || strcmp(info.name, "lingerer") == 0) {
        printf("%s %s\n", what, info.name);
        fflush(stdout);
    }
    (*jvmti)->Deallocate(jvmti, (unsigned char *)info.name);
}

static void JNICALL
on_thread_start(jvmtiEnv *jvmti, JNIEnv *env, jthread thread)
{
    (void)env;
    report(jvmti, thread, "ThreadStart");
}

static void JNICALL
on_thread_end(jvmtiEnv *jvmti, JNIEnv *env, jthread thread)
{
    (void)env;
    report(jvmti, thread, "ThreadEnd");
}

JNIEXPORT jint JNICALL
Agent_OnLoad(JavaVM *vm, char *options, void *reserved)
{
    jvmtiEnv *jvmti = NULL;
    jvmtiEventCallbacks callbacks;
    (void)options;
    (void)reserved;
    if ((*vm)->GetEnv(vm, (void **)&jvmti, JVMTI_VERSION_1_2) != JNI_OK) {
        return 1;
    }
    memset(&callbacks, 0, sizeof callbacks);
    callbacks.ThreadStart = on_thread_start;
    callbacks.ThreadEnd = on_thread_end;
    if ((*jvmti)->SetEventCallbacks(jvmti, &callbacks, (jint)sizeof callbacks)
        != JVMTI_ERROR_NONE) {
        return 1;
    }
    (*jvmti)->SetEventNotificationMode(jvmti, JVMTI_ENABLE, JVMTI_EVENT_THREAD_START, NULL);
    (*jvmti)->SetEventNotificationMode(jvmti, JVMTI_ENABLE, JVMTI_EVENT_THREAD_END, NULL);
    return 0;
}
