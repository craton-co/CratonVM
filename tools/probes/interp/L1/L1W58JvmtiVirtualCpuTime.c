/*
 * Native shim and agent for tools/probes/interp/L1/L1W58JvmtiVirtualCpuTime.java
 * (interpreter round i2 wave 58, lane L1). Loaded twice: as an agent
 * (`-agentpath:...`), whose `Agent_OnLoad` keeps the JVMTI env and adds the
 * two CPU-time capabilities, and with `System.load`, for the natives. Build
 * instructions are in the probe's header; the Rust port
 * tools/probes/interp/L1/rust-ports/L1W58JvmtiVirtualCpuTime prints the same
 * lines.
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
    jvmtiCapabilities caps;
    (void)options;
    (void)reserved;
    if ((*vm)->GetEnv(vm, (void **)&jvmti, JVMTI_VERSION_1_2) != JNI_OK) {
        return 1;
    }
    memset(&caps, 0, sizeof caps);
    caps.can_get_current_thread_cpu_time = 1;
    caps.can_get_thread_cpu_time = 1;
    (*jvmti)->AddCapabilities(jvmti, &caps);
    agent_env = jvmti;
    return 0;
}

static jstring
err_line(JNIEnv *env, jstring label, int err)
{
    const char *text = (*env)->GetStringUTFChars(env, label, NULL);
    char line[256];
    snprintf(line, sizeof line, "%s: err=%d\n", text, err);
    (*env)->ReleaseStringUTFChars(env, label, text);
    return (*env)->NewStringUTF(env, line);
}

/* "<label>: err=<e>" of `GetThreadCpuTime(t)` (NULL: the current thread). */
JNIEXPORT jstring JNICALL
Java_L1W58JvmtiVirtualCpuTime_cpu(JNIEnv *env, jclass self, jstring label, jthread t)
{
    jlong nanos = 0;
    (void)self;
    if (agent_env == NULL) {
        return (*env)->NewStringUTF(env, "no JVMTI env (load the shim with -agentpath too)\n");
    }
    return err_line(env, label, (int)(*agent_env)->GetThreadCpuTime(agent_env, t, &nanos));
}

/* "<label>: err=<e>" of `GetCurrentThreadCpuTime`. */
JNIEXPORT jstring JNICALL
Java_L1W58JvmtiVirtualCpuTime_currentCpu(JNIEnv *env, jclass self, jstring label)
{
    jlong nanos = 0;
    (void)self;
    if (agent_env == NULL) {
        return (*env)->NewStringUTF(env, "no JVMTI env (load the shim with -agentpath too)\n");
    }
    return err_line(env, label, (int)(*agent_env)->GetCurrentThreadCpuTime(agent_env, &nanos));
}
