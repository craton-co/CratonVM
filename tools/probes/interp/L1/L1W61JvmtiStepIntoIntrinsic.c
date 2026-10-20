/*
 * Native shim and agent for tools/probes/interp/L1/L1W61JvmtiStepIntoIntrinsic.java
 * (interpreter round i2 wave 61, lane L1). Loaded twice: as an agent
 * (`-agentpath`), whose `Agent_OnLoad` adds `can_generate_single_step_events`
 * and sets the `SingleStep` callback, and with `System.load`, for the
 * natives. Build instructions are in the probe's header; the Rust port is
 * tools/probes/interp/L1/rust-ports/L1W61JvmtiStepIntoIntrinsic.
 */
#include <jni.h>
#include <jvmti.h>
#include <stdio.h>
#include <string.h>

#define MAX_METHODS 64

static jvmtiEnv *agent_env = NULL;
static char names[MAX_METHODS][64];
static int counts[MAX_METHODS];
static int used = 0;

static void JNICALL
on_single_step(jvmtiEnv *jvmti, JNIEnv *env, jthread thread, jmethodID method, jlocation location)
{
    char *name = NULL;
    int i;
    (void)env;
    (void)thread;
    (void)location;
    if ((*jvmti)->GetMethodName(jvmti, method, &name, NULL, NULL) != JVMTI_ERROR_NONE || name == NULL) {
        return;
    }
    for (i = 0; i < used; i++) {
        if (strcmp(names[i], name) == 0) {
            counts[i]++;
            break;
        }
    }
    if (i == used && used < MAX_METHODS) {
        snprintf(names[used], sizeof names[used], "%s", name);
        counts[used] = 1;
        used++;
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
Java_L1W61JvmtiStepIntoIntrinsic_startStep(JNIEnv *env, jclass self)
{
    jthread current = NULL;
    (void)env;
    (void)self;
    if (agent_env == NULL) {
        return -1;
    }
    used = 0;
    (*agent_env)->GetCurrentThread(agent_env, &current);
    return (jint)(*agent_env)->SetEventNotificationMode(agent_env, JVMTI_ENABLE, JVMTI_EVENT_SINGLE_STEP, current);
}

JNIEXPORT jstring JNICALL
Java_L1W61JvmtiStepIntoIntrinsic_stopStep(JNIEnv *env, jclass self)
{
    jthread current = NULL;
    char out[4096];
    char item[96];
    int i;
    (void)self;
    if (agent_env == NULL) {
        return (*env)->NewStringUTF(env, "no JVMTI env (load the shim with -agentpath too)");
    }
    (*agent_env)->GetCurrentThread(agent_env, &current);
    (*agent_env)->SetEventNotificationMode(agent_env, JVMTI_DISABLE, JVMTI_EVENT_SINGLE_STEP, current);
    out[0] = '\0';
    for (i = 0; i < used; i++) {
        snprintf(item, sizeof item, "%s=%d ", names[i], counts[i]);
        if (strlen(out) + strlen(item) + 1 < sizeof out) {
            strcat(out, item);
        }
    }
    used = 0;
    return (*env)->NewStringUTF(env, out);
}
