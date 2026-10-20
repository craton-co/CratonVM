/*
 * Native shim and agent for tools/probes/interp/L1/L1W56JvmtiReentryContended.java
 * (interpreter round i2 wave 56, lane L1). Loaded twice: as an agent
 * (`-agentpath:...`), whose `Agent_OnLoad` adds
 * `can_get_current_contended_monitor` (an OnLoad-only capability), and with
 * `System.load`, for the native `contended`. Build instructions are in the
 * probe's header; the Rust port
 * tools/probes/interp/L1/rust-ports/L1W56JvmtiReentryContended prints the same lines.
 */
#include <jni.h>
#include <jvmti.h>
#include <stdio.h>
#include <string.h>

static jvmtiEnv *agent_env = NULL;
static int onload_add = -1;

/* The `jvmtiThreadState` bits the probe compares: ALIVE, RUNNABLE,
 * WAITING, BLOCKED_ON_MONITOR_ENTER. */
#define STATE_MASK 0x485

JNIEXPORT jint JNICALL
Agent_OnLoad(JavaVM *vm, char *options, void *reserved)
{
    jvmtiEnv *jvmti = NULL;
    jvmtiCapabilities wanted;
    (void)options;
    (void)reserved;
    if ((*vm)->GetEnv(vm, (void **)&jvmti, JVMTI_VERSION_1_2) != JNI_OK) {
        return 1;
    }
    memset(&wanted, 0, sizeof wanted);
    wanted.can_get_current_contended_monitor = 1;
    onload_add = (int)(*jvmti)->AddCapabilities(jvmti, &wanted);
    agent_env = jvmti;
    return 0;
}

/* "<label>: err=<e> monitor=<null|lock|other> state=0x<bits>" for thread
 * `t`, whose expected monitor is `lock`. */
JNIEXPORT jstring JNICALL
Java_L1W56JvmtiReentryContended_contended(JNIEnv *env, jclass self, jstring label, jthread t, jobject lock)
{
    jvmtiEnv *jvmti = agent_env;
    jobject monitor = NULL;
    jint state = 0;
    const char *text;
    const char *which;
    char line[256];
    int err;
    (void)self;
    if (jvmti == NULL) {
        return (*env)->NewStringUTF(env, "no JVMTI env (load the shim with -agentpath too)\n");
    }
    err = (int)(*jvmti)->GetCurrentContendedMonitor(jvmti, t, &monitor);
    (*jvmti)->GetThreadState(jvmti, t, &state);
    if (monitor == NULL) {
        which = "null";
    } else if ((*env)->IsSameObject(env, monitor, lock)) {
        which = "lock";
    } else {
        which = "other";
    }
    text = (*env)->GetStringUTFChars(env, label, NULL);
    snprintf(line, sizeof line, "%s: onload=%d err=%d monitor=%s state=0x%x\n", text, onload_add, err,
             which, (unsigned)(state & STATE_MASK));
    (*env)->ReleaseStringUTFChars(env, label, text);
    return (*env)->NewStringUTF(env, line);
}
