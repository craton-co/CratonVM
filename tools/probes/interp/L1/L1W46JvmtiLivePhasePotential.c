/*
 * Native shim and agent for tools/probes/interp/L1/L1W46JvmtiLivePhasePotential.java
 * (interpreter round i1 wave 46, lane L1). Loaded twice: as an agent
 * (`-agentpath:...=<none|locals|all>`), whose `Agent_OnLoad` acquires the
 * capabilities the option names, and with `System.load`, for the native
 * `rows()`. Build instructions are in the probe's header.
 */
#include <jni.h>
#include <jvmti.h>
#include <stdarg.h>
#include <stdio.h>
#include <string.h>

static char out[4096];
static jvmtiEnv *agent_env = NULL;
static char option[16] = "none";
static int onload_add = -1;

JNIEXPORT jint JNICALL
Agent_OnLoad(JavaVM *vm, char *options, void *reserved)
{
    jvmtiEnv *jvmti = NULL;
    jvmtiCapabilities wanted;
    (void)reserved;
    if (options != NULL) {
        snprintf(option, sizeof option, "%s", options);
    }
    if ((*vm)->GetEnv(vm, (void **)&jvmti, JVMTI_VERSION_1_2) != JNI_OK) {
        return 1;
    }
    memset(&wanted, 0, sizeof wanted);
    if (strcmp(option, "locals") == 0 || strcmp(option, "all") == 0) {
        wanted.can_access_local_variables = 1;
    }
    if (strcmp(option, "all") == 0) {
        wanted.can_get_current_contended_monitor = 1;
        wanted.can_get_owned_monitor_stack_depth_info = 1;
    }
    onload_add = (int)(*jvmti)->AddCapabilities(jvmti, &wanted);
    agent_env = jvmti;
    return 0;
}

static void line(const char *fmt, ...)
{
    char item[256];
    va_list ap;
    va_start(ap, fmt);
    vsnprintf(item, sizeof item, fmt, ap);
    va_end(ap);
    if (strlen(out) + strlen(item) + 2 < sizeof out) {
        strcat(out, item);
        strcat(out, "\n");
    }
}

/* The potential bits of the three and `AddCapabilities` of each alone, for
   a new env obtained now. */
static void live_env_rows(JNIEnv *env, const char *row)
{
    JavaVM *vm = NULL;
    jvmtiEnv *live = NULL;
    jvmtiCapabilities potential;
    jvmtiCapabilities one;
    int add_locals;
    int add_contended;
    int add_owned;
    (*env)->GetJavaVM(env, &vm);
    if ((*vm)->GetEnv(vm, (void **)&live, JVMTI_VERSION_1_2) != JNI_OK) {
        line("%s: no env", row);
        return;
    }
    memset(&potential, 0, sizeof potential);
    (*live)->GetPotentialCapabilities(live, &potential);
    memset(&one, 0, sizeof one);
    one.can_access_local_variables = 1;
    add_locals = (int)(*live)->AddCapabilities(live, &one);
    memset(&one, 0, sizeof one);
    one.can_get_current_contended_monitor = 1;
    add_contended = (int)(*live)->AddCapabilities(live, &one);
    memset(&one, 0, sizeof one);
    one.can_get_owned_monitor_stack_depth_info = 1;
    add_owned = (int)(*live)->AddCapabilities(live, &one);
    line("%s: potential %d %d %d add %d %d %d", row, (int)potential.can_access_local_variables,
         (int)potential.can_get_current_contended_monitor,
         (int)potential.can_get_owned_monitor_stack_depth_info, add_locals, add_contended,
         add_owned);
    (*live)->DisposeEnvironment(live);
}

JNIEXPORT jstring JNICALL
Java_L1W46JvmtiLivePhasePotential_rows(JNIEnv *env, jclass self)
{
    jvmtiCapabilities held;
    (void)self;
    out[0] = '\0';
    if (agent_env == NULL) {
        return (*env)->NewStringUTF(env, "no JVMTI env (load the shim with -agentpath too)\n");
    }
    line("option %s: Agent_OnLoad AddCapabilities: err=%d", option, onload_add);
    live_env_rows(env, "live-phase env");
    memset(&held, 0, sizeof held);
    (*agent_env)->GetCapabilities(agent_env, &held);
    line("startup env relinquishes: err=%d",
         (int)(*agent_env)->RelinquishCapabilities(agent_env, &held));
    live_env_rows(env, "live-phase env after the relinquish");
    return (*env)->NewStringUTF(env, out);
}
