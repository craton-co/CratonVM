/*
 * Native shim and agent for tools/probes/interp/L1/L1W62JvmtiCallbackFrameControl.java
 * (interpreter round i2 wave 62, lane L1). Loaded twice: as an agent
 * (`-agentpath`), whose `Agent_OnLoad` adds `can_pop_frame`,
 * `can_force_early_return` and the three event capabilities and sets the
 * callbacks, and with `System.load`, for the natives. Build instructions are
 * in the probe's header; the Rust port is
 * tools/probes/interp/L1/rust-ports/L1W62JvmtiCallbackFrameControl.
 */
#include <jni.h>
#include <jvmti.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static jvmtiEnv *agent_env = NULL;
static int armed_event = 0;
static char target_name[64];
static jlong target_location = -1;
static char actions[256];
static jmethodID bp_method = NULL;
static jlocation bp_location = 0;
static int hits = 0;
static int acted = 0;
static char record[512];

static void
append(const char *item)
{
    if (strlen(record) + strlen(item) + 1 < sizeof record) {
        strcat(record, item);
    }
}

static void
act(jvmtiEnv *jvmti, JNIEnv *env, jthread thread, jmethodID method, jlocation location)
{
    char *name = NULL;
    char copy[256];
    char item[64];
    char *token;
    int matched;
    if ((*jvmti)->GetMethodName(jvmti, method, &name, NULL, NULL) != JVMTI_ERROR_NONE || name == NULL) {
        return;
    }
    matched = strcmp(name, target_name) == 0 && (target_location < 0 || location == target_location);
    (*jvmti)->Deallocate(jvmti, (unsigned char *)name);
    if (!matched) {
        return;
    }
    hits++;
    if (acted) {
        return;
    }
    acted = 1;
    snprintf(copy, sizeof copy, "%s", actions);
    for (token = strtok(copy, " "); token != NULL; token = strtok(NULL, " ")) {
        jvmtiError err;
        if (strncmp(token, "int:", 4) == 0) {
            err = (*jvmti)->ForceEarlyReturnInt(jvmti, thread, atoi(token + 4));
        } else if (strncmp(token, "intnull:", 8) == 0) {
            err = (*jvmti)->ForceEarlyReturnInt(jvmti, NULL, atoi(token + 8));
        } else if (strcmp(token, "long") == 0) {
            err = (*jvmti)->ForceEarlyReturnLong(jvmti, thread, 77);
        } else if (strcmp(token, "obj") == 0) {
            err = (*jvmti)->ForceEarlyReturnObject(jvmti, thread, (*env)->NewStringUTF(env, "forced"));
        } else if (strcmp(token, "void") == 0) {
            err = (*jvmti)->ForceEarlyReturnVoid(jvmti, thread);
        } else if (strcmp(token, "pop") == 0) {
            err = (*jvmti)->PopFrame(jvmti, thread);
        } else {
            err = -1;
        }
        snprintf(item, sizeof item, "%s=%d ", token, (int)err);
        append(item);
    }
}

static void JNICALL
on_breakpoint(jvmtiEnv *jvmti, JNIEnv *env, jthread thread, jmethodID method, jlocation location)
{
    if (armed_event == 1) {
        act(jvmti, env, thread, method, location);
    }
}

static void JNICALL
on_single_step(jvmtiEnv *jvmti, JNIEnv *env, jthread thread, jmethodID method, jlocation location)
{
    if (armed_event == 2) {
        act(jvmti, env, thread, method, location);
    }
}

static void JNICALL
on_method_entry(jvmtiEnv *jvmti, JNIEnv *env, jthread thread, jmethodID method)
{
    if (armed_event == 3) {
        act(jvmti, env, thread, method, -1);
    }
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
    wanted.can_pop_frame = 1;
    wanted.can_force_early_return = 1;
    wanted.can_generate_breakpoint_events = 1;
    wanted.can_generate_single_step_events = 1;
    wanted.can_generate_method_entry_events = 1;
    (*jvmti)->AddCapabilities(jvmti, &wanted);
    memset(&callbacks, 0, sizeof callbacks);
    callbacks.Breakpoint = on_breakpoint;
    callbacks.SingleStep = on_single_step;
    callbacks.MethodEntry = on_method_entry;
    (*jvmti)->SetEventCallbacks(jvmti, &callbacks, (jint)sizeof callbacks);
    agent_env = jvmti;
    return 0;
}

JNIEXPORT jint JNICALL
Java_L1W62JvmtiCallbackFrameControl_arm(JNIEnv *env, jclass self, jint event, jstring method, jstring sig,
                                        jlong location, jstring acts)
{
    const char *m;
    const char *s;
    const char *a;
    jthread current = NULL;
    jvmtiError err;
    if (agent_env == NULL) {
        return -1;
    }
    m = (*env)->GetStringUTFChars(env, method, NULL);
    s = (*env)->GetStringUTFChars(env, sig, NULL);
    a = (*env)->GetStringUTFChars(env, acts, NULL);
    snprintf(target_name, sizeof target_name, "%s", m);
    snprintf(actions, sizeof actions, "%s", a);
    target_location = location;
    hits = 0;
    acted = 0;
    record[0] = '\0';
    bp_method = NULL;
    armed_event = event;
    (*agent_env)->GetCurrentThread(agent_env, &current);
    if (event == 1) {
        bp_method = (*env)->GetStaticMethodID(env, self, m, s);
        bp_location = (jlocation)location;
        err = (*agent_env)->SetBreakpoint(agent_env, bp_method, bp_location);
        if (err == JVMTI_ERROR_NONE) {
            err = (*agent_env)->SetEventNotificationMode(agent_env, JVMTI_ENABLE, JVMTI_EVENT_BREAKPOINT, current);
        }
    } else if (event == 2) {
        err = (*agent_env)->SetEventNotificationMode(agent_env, JVMTI_ENABLE, JVMTI_EVENT_SINGLE_STEP, current);
    } else {
        err = (*agent_env)->SetEventNotificationMode(agent_env, JVMTI_ENABLE, JVMTI_EVENT_METHOD_ENTRY, current);
    }
    (*env)->ReleaseStringUTFChars(env, method, m);
    (*env)->ReleaseStringUTFChars(env, sig, s);
    (*env)->ReleaseStringUTFChars(env, acts, a);
    return (jint)err;
}

JNIEXPORT jstring JNICALL
Java_L1W62JvmtiCallbackFrameControl_disarm(JNIEnv *env, jclass self)
{
    jthread current = NULL;
    char out[640];
    (void)self;
    if (agent_env == NULL) {
        return (*env)->NewStringUTF(env, "no JVMTI env (load the shim with -agentpath too)");
    }
    (*agent_env)->GetCurrentThread(agent_env, &current);
    if (armed_event == 1) {
        (*agent_env)->SetEventNotificationMode(agent_env, JVMTI_DISABLE, JVMTI_EVENT_BREAKPOINT, current);
        (*agent_env)->ClearBreakpoint(agent_env, bp_method, bp_location);
    } else if (armed_event == 2) {
        (*agent_env)->SetEventNotificationMode(agent_env, JVMTI_DISABLE, JVMTI_EVENT_SINGLE_STEP, current);
    } else {
        (*agent_env)->SetEventNotificationMode(agent_env, JVMTI_DISABLE, JVMTI_EVENT_METHOD_ENTRY, current);
    }
    armed_event = 0;
    snprintf(out, sizeof out, "%shits=%d", record, hits);
    return (*env)->NewStringUTF(env, out);
}
