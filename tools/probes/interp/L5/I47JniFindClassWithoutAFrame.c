/*
 * Native agent for tools/probes/interp/L5/I47JniFindClassWithoutAFrame.java
 * (item 5 of docs/internal/fixed-bugs/interpreter-L5-getinitiatedclasses-residual-recording-differences-FIXED-20261007.md,
 * 2026-10-07). Its `VMInit` callback runs on a thread with no Java frame and
 * calls JNI `FindClass` on a JDK class nobody has loaded; HotSpot resolves
 * such a `FindClass` through the system class loader, which becomes an
 * initiating loader of it. Build instructions are in the probe's header.
 */
#include <jni.h>
#include <jvmti.h>
#include <string.h>

static void JNICALL vm_init(jvmtiEnv *jvmti, JNIEnv *env, jthread thread)
{
    jclass found;
    (void)jvmti;
    (void)thread;
    found = (*env)->FindClass(env, "java/util/concurrent/Exchanger");
    if (found == NULL) {
        (*env)->ExceptionClear(env);
    }
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
    callbacks.VMInit = &vm_init;
    if ((*jvmti)->SetEventCallbacks(jvmti, &callbacks, (jint)sizeof callbacks) != JVMTI_ERROR_NONE) {
        return 1;
    }
    if ((*jvmti)->SetEventNotificationMode(jvmti, JVMTI_ENABLE, JVMTI_EVENT_VM_INIT, NULL)
            != JVMTI_ERROR_NONE) {
        return 1;
    }
    return 0;
}
