/*
 * Native shim and agent for tools/probes/interp/L1/L1W81JvmtiStepLatency.java
 * (interpreter round i3 wave 81, lane L1). Loaded twice: as an agent
 * (`-agentpath`), whose `Agent_OnLoad` adds `can_generate_single_step_events`
 * and sets the `SingleStep` callback, and with `System.load`, for the
 * natives. `startStep` enables `SingleStep` for the calling thread;
 * `stopStep` disables it and answers the steps posted in between, as
 * `<method>@<location>`, up to and including the first step in `target`.
 */
#include <jni.h>
#include <jvmti.h>
#include <stdio.h>
#include <string.h>

#define MAX_STEPS 32

static jvmtiEnv *agent_env = NULL;
static char steps[MAX_STEPS][80];
static int used = 0;
static int done = 0;

static void JNICALL
on_single_step(jvmtiEnv *jvmti, JNIEnv *env, jthread thread, jmethodID method, jlocation location)
{
    char *name = NULL;
    (void)env;
    (void)thread;
    if (done || used >= MAX_STEPS) {
        return;
    }
    if ((*jvmti)->GetMethodName(jvmti, method, &name, NULL, NULL) != JVMTI_ERROR_NONE || name == NULL) {
        return;
    }
    snprintf(steps[used], sizeof steps[used], "%s@%lld", name, (long long)location);
    used++;
    if (strcmp(name, "target") == 0) {
        done = 1;
    }
    (*jvmti)->Deallocate(jvmti, (unsigned char *)name);
}

JNIEXPORT jint JNICALL
Agent_OnLoad(JavaVM *vm, char *options, void *reserved)
{
    jvmtiEnv *jvmti = NULL;
    jvmtiCapabilities wanted;
    jvmtiEventCallbacks callbacks;
    (void)options;
    (void)reserved;
    if ((*vm)->GetEnv(vm, (void **)&jvmti, JVMTI_VERSION_1_2) != JNI_OK) {
        return 1;
    }
    memset(&wanted, 0, sizeof wanted);
    wanted.can_generate_single_step_events = 1;
    (*jvmti)->AddCapabilities(jvmti, &wanted);
    memset(&callbacks, 0, sizeof callbacks);
    callbacks.SingleStep = on_single_step;
    (*jvmti)->SetEventCallbacks(jvmti, &callbacks, (jint)sizeof callbacks);
    agent_env = jvmti;
    return 0;
}

JNIEXPORT jint JNICALL
Java_L1W81JvmtiStepLatency_startStep(JNIEnv *env, jclass self)
{
    jthread current = NULL;
    (void)env;
    (void)self;
    if (agent_env == NULL) {
        return -1;
    }
    used = 0;
    done = 0;
    (*agent_env)->GetCurrentThread(agent_env, &current);
    return (jint)(*agent_env)->SetEventNotificationMode(agent_env, JVMTI_ENABLE, JVMTI_EVENT_SINGLE_STEP, current);
}

JNIEXPORT jstring JNICALL
Java_L1W81JvmtiStepLatency_stopStep(JNIEnv *env, jclass self)
{
    jthread current = NULL;
    char out[4096];
    int i;
    (void)self;
    if (agent_env == NULL) {
        return (*env)->NewStringUTF(env, "no JVMTI env (load the shim with -agentpath too)");
    }
    (*agent_env)->GetCurrentThread(agent_env, &current);
    (*agent_env)->SetEventNotificationMode(agent_env, JVMTI_DISABLE, JVMTI_EVENT_SINGLE_STEP, current);
    out[0] = '\0';
    for (i = 0; i < used; i++) {
        if (strlen(out) + strlen(steps[i]) + 2 < sizeof out) {
            if (i > 0) {
                strcat(out, " ");
            }
            strcat(out, steps[i]);
        }
    }
    used = 0;
    done = 0;
    return (*env)->NewStringUTF(env, out);
}
