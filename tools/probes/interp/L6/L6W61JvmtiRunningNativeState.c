/*
 * Native shim and agent for tools/probes/interp/L6/L6W61JvmtiRunningNativeState.java
 * (interpreter round i2 wave 61, lane L6). Loaded twice: as an agent
 * (`-agentpath:...`), whose `Agent_OnLoad` keeps the JVMTI env, and with
 * `System.load`, for the native. Build instructions are in the probe's
 * header; the Rust port tools/probes/interp/L6/rust-ports/L6W61JvmtiRunningNativeState
 * prints the same lines.
 */
#include <jni.h>
#include <jvmti.h>
#include <stdio.h>
#include <string.h>

static jvmtiEnv *agent_env = NULL;

JNIEXPORT jint JNICALL
Agent_OnLoad(JavaVM *vm, char *options, void *reserved)
{
    jvmtiEnv *jvmti = NULL;
    (void)options;
    (void)reserved;
    if ((*vm)->GetEnv(vm, (void **)&jvmti, JVMTI_VERSION_1_2) != JNI_OK) {
        return 1;
    }
    agent_env = jvmti;
    return 0;
}

/* "<label>: err=<e> state=0x<the whole word>" for thread `t`. */
JNIEXPORT jstring JNICALL
Java_L6W61JvmtiRunningNativeState_state(JNIEnv *env, jclass self, jstring label, jthread t)
{
    jvmtiEnv *jvmti = agent_env;
    jint state = 0;
    const char *text;
    char line[256];
    int err;
    (void)self;
    if (jvmti == NULL) {
        return (*env)->NewStringUTF(env, "no JVMTI env (load the shim with -agentpath too)\n");
    }
    err = (int)(*jvmti)->GetThreadState(jvmti, t, &state);
    text = (*env)->GetStringUTFChars(env, label, NULL);
    snprintf(line, sizeof line, "%s: err=%d state=0x%x\n", text, err, (unsigned)state);
    (*env)->ReleaseStringUTFChars(env, label, text);
    return (*env)->NewStringUTF(env, line);
}
