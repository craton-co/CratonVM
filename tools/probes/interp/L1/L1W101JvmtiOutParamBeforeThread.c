/*
 * Native shim and agent for tools/probes/interp/L1/L1W101JvmtiOutParamBeforeThread.java
 * (interpreter round i5 wave 101, lane L1). Loaded twice: as an agent
 * (`-agentpath:...`), whose `Agent_OnLoad` adds the capabilities, and with
 * `System.load`, for the native `rows`. Build instructions are in the
 * probe's header.
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
    caps.can_get_owned_monitor_info = 1;
    caps.can_get_owned_monitor_stack_depth_info = 1;
    caps.can_get_current_contended_monitor = 1;
    if ((*jvmti)->AddCapabilities(jvmti, &caps) != JVMTI_ERROR_NONE) {
        return 1;
    }
    agent_env = jvmti;
    return 0;
}

#define ROW(label, call)                                                        \
    do {                                                                        \
        int err_ = (int)(call);                                                 \
        if (used < sizeof out) {                                                \
            used += (size_t)snprintf(out + used, sizeof out - used,             \
                                     "%s %s: err=%d\n", who, label, err_);      \
        }                                                                       \
    } while (0)

/*
 * For each of `notThread` (a String), `dead` (a terminated Thread) and
 * `live` (a running Thread): the functions whose out-parameters HotSpot's
 * generated entry pre-checks, called with NULL out-parameters.
 */
JNIEXPORT jstring JNICALL
Java_L1W101JvmtiOutParamBeforeThread_rows(JNIEnv *env, jclass self, jobject notThread,
                                          jobject dead, jobject live)
{
    jvmtiEnv *jvmti = agent_env;
    static char out[8192];
    size_t used = 0;
    jobject targets[3];
    const char *names[3] = {"not-a-thread", "dead", "live"};
    int i;
    (void)self;
    if (jvmti == NULL) {
        return (*env)->NewStringUTF(env, "no JVMTI env (load the shim with -agentpath too)\n");
    }
    targets[0] = notThread;
    targets[1] = dead;
    targets[2] = live;
    out[0] = '\0';
    for (i = 0; i < 3; i++) {
        jthread t = (jthread)targets[i];
        const char *who = names[i];
        ROW("GetThreadInfo NULL", (*jvmti)->GetThreadInfo(jvmti, t, NULL));
        ROW("GetThreadState NULL", (*jvmti)->GetThreadState(jvmti, t, NULL));
        ROW("GetCurrentContendedMonitor NULL",
            (*jvmti)->GetCurrentContendedMonitor(jvmti, t, NULL));
        ROW("GetOwnedMonitorInfo NULL", (*jvmti)->GetOwnedMonitorInfo(jvmti, t, NULL, NULL));
        ROW("GetOwnedMonitorStackDepthInfo NULL",
            (*jvmti)->GetOwnedMonitorStackDepthInfo(jvmti, t, NULL, NULL));
        ROW("GetFrameCount NULL", (*jvmti)->GetFrameCount(jvmti, t, NULL));
        ROW("GetFrameLocation NULL", (*jvmti)->GetFrameLocation(jvmti, t, 0, NULL, NULL));
        ROW("GetFrameLocation -1", (*jvmti)->GetFrameLocation(jvmti, t, -1, NULL, NULL));
    }
    return (*env)->NewStringUTF(env, out);
}
