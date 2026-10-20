/*
 * Native shim and agent for tools/probes/interp/L1/L1W51JvmtiUnmountedVirtualLocals.java
 * (interpreter round i1 wave 51, lane L1). Loaded twice: as an agent
 * (`-agentpath`), whose `Agent_OnLoad` adds `can_suspend`,
 * `can_access_local_variables` and `can_get_owned_monitor_stack_depth_info`, and with
 * `System.load`, for the native `rows(...)`, which reads the locals and the
 * owned monitors of two suspended, unmounted virtual threads. Build
 * instructions are in the probe's header.
 */
#include <jni.h>
#include <jvmti.h>
#include <stdarg.h>
#include <stdio.h>
#include <string.h>

static char out[8192];
static jvmtiEnv *agent_env = NULL;

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
    wanted.can_suspend = 1;
    wanted.can_access_local_variables = 1;
    wanted.can_get_owned_monitor_stack_depth_info = 1;
    if ((*jvmti)->AddCapabilities(jvmti, &wanted) != JVMTI_ERROR_NONE) {
        return 1;
    }
    /* On its own: a VM without it still runs the rows above it. */
    memset(&wanted, 0, sizeof wanted);
    wanted.can_get_owned_monitor_info = 1;
    (void)(*jvmti)->AddCapabilities(jvmti, &wanted);
    agent_env = jvmti;
    return 0;
}

static void line(const char *fmt, ...)
{
    char item[1024];
    va_list ap;
    size_t used = strlen(out);
    va_start(ap, fmt);
    vsnprintf(item, sizeof item, fmt, ap);
    va_end(ap);
    if (used + strlen(item) + 2 < sizeof out) {
        strcat(out, item);
        strcat(out, "\n");
    }
}

/* The depth of the topmost frame of `thread` whose method is named `stop`, or -1. */
static jint depth_of(jvmtiEnv *jvmti, jthread thread, const char *stop)
{
    jvmtiFrameInfo frames[32];
    jint n = -1;
    jint k;
    if ((*jvmti)->GetStackTrace(jvmti, thread, 0, 32, frames, &n) != JVMTI_ERROR_NONE) {
        return -1;
    }
    for (k = 0; k < n && k < 32; k++) {
        char *name = NULL;
        int same = 0;
        if ((*jvmti)->GetMethodName(jvmti, frames[k].method, &name, NULL, NULL) == JVMTI_ERROR_NONE
            && name != NULL) {
            same = strcmp(name, stop) == 0;
            (*jvmti)->Deallocate(jvmti, (unsigned char *)name);
        }
        if (same) {
            return k;
        }
    }
    return -1;
}

static void thread_rows(JNIEnv *env, jvmtiEnv *jvmti, const char *label, jthread thread, const char *stop)
{
    jint depth;
    jint ival = -1;
    jobject oval = NULL;
    jint count = -1;
    jvmtiMonitorStackDepthInfo *monitors = NULL;
    jvmtiError err = (*jvmti)->SuspendThread(jvmti, thread);
    line("SuspendThread %s: err=%d", label, (int)err);
    depth = depth_of(jvmti, thread, stop);
    line("frame %s: %s", label, depth < 0 ? "missing" : "found");
    if (depth >= 0) {
        err = (*jvmti)->GetLocalInt(jvmti, thread, depth, 0, &ival);
        if (err == JVMTI_ERROR_NONE) {
            line("GetLocalInt %s n: err=0 %d", label, (int)ival);
        } else {
            line("GetLocalInt %s n: err=%d", label, (int)err);
        }
        err = (*jvmti)->GetLocalObject(jvmti, thread, depth, 1, &oval);
        if (err == JVMTI_ERROR_NONE && oval != NULL) {
            const char *chars = (*env)->GetStringUTFChars(env, (jstring)oval, NULL);
            line("GetLocalObject %s tag: err=0 %s", label, chars != NULL ? chars : "?");
            if (chars != NULL) {
                (*env)->ReleaseStringUTFChars(env, (jstring)oval, chars);
            }
        } else if (err == JVMTI_ERROR_NONE) {
            line("GetLocalObject %s tag: err=0 null", label);
        } else {
            line("GetLocalObject %s tag: err=%d", label, (int)err);
        }
        err = (*jvmti)->GetLocalInt(jvmti, thread, depth, 9, &ival);
        line("GetLocalInt %s slot 9: err=%d", label, (int)err);
        oval = NULL;
        err = (*jvmti)->GetLocalInstance(jvmti, thread, depth, &oval);
        line("GetLocalInstance %s: err=%d", label, (int)err);
        err = (*jvmti)->SetLocalInt(jvmti, thread, depth, 0, 8);
        line("SetLocalInt %s n: err=%d", label, (int)err);
    }
    err = (*jvmti)->GetOwnedMonitorStackDepthInfo(jvmti, thread, &count, &monitors);
    if (err == JVMTI_ERROR_NONE) {
        line("GetOwnedMonitorStackDepthInfo %s: err=0 count=%d", label, (int)count);
        (*jvmti)->Deallocate(jvmti, (unsigned char *)monitors);
    } else {
        line("GetOwnedMonitorStackDepthInfo %s: err=%d", label, (int)err);
    }
    err = (*jvmti)->ResumeThread(jvmti, thread);
    line("ResumeThread %s: err=%d", label, (int)err);
}

JNIEXPORT jstring JNICALL
Java_L1W51JvmtiUnmountedVirtualLocals_rows(JNIEnv *env, jclass self, jthread sleeper, jthread parker,
                                            jobject lock)
{
    jvmtiEnv *jvmti = agent_env;
    jint count = -1;
    jobject *monitors = NULL;
    jvmtiError err;
    (void)self;
    out[0] = '\0';
    if (jvmti == NULL) {
        return (*env)->NewStringUTF(env, "no JVMTI env (load the shim with -agentpath too)\n");
    }
    thread_rows(env, jvmti, "sleeper", sleeper, "sleepIn");
    thread_rows(env, jvmti, "parker", parker, "parkIn");
    /* GetOwnedMonitorInfo (slot 10) of the calling thread, which holds `lock`. */
    err = (*jvmti)->GetOwnedMonitorInfo(jvmti, NULL, &count, &monitors);
    if (err == JVMTI_ERROR_NONE) {
        int has_lock = 0;
        jint k;
        for (k = 0; k < count; k++) {
            if ((*env)->IsSameObject(env, monitors[k], lock)) {
                has_lock = 1;
            }
        }
        line("GetOwnedMonitorInfo current: err=0 count=%d lock=%s", (int)count, has_lock ? "true" : "false");
        (*jvmti)->Deallocate(jvmti, (unsigned char *)monitors);
    } else {
        line("GetOwnedMonitorInfo current: err=%d", (int)err);
    }
    return (*env)->NewStringUTF(env, out);
}
