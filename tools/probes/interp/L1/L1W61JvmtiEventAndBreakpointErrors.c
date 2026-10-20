/*
 * Native shim and agent for
 * tools/probes/interp/L1/L1W61JvmtiEventAndBreakpointErrors.java (interpreter
 * round i2 wave 61, lane L1). Loaded twice: as an agent (`-agentpath`),
 * whose `Agent_OnLoad` records the OnLoad-phase rows and adds
 * `can_generate_breakpoint_events`, and with `System.load`, for the natives.
 * Build instructions are in the probe's header; the Rust port is
 * tools/probes/interp/L1/rust-ports/L1W61JvmtiEventAndBreakpointErrors.
 */
#include <jni.h>
#include <jvmti.h>
#include <stdarg.h>
#include <stdio.h>
#include <string.h>

static char onload_out[1024];
static jvmtiEnv *agent_env = NULL;

static void append(char *out, size_t size, const char *fmt, ...)
{
    char item[256];
    va_list ap;
    size_t used = strlen(out);
    va_start(ap, fmt);
    vsnprintf(item, sizeof item, fmt, ap);
    va_end(ap);
    if (used + strlen(item) + 2 < size) {
        strcat(out, item);
        strcat(out, "\n");
    }
}

#define ROW(out, row, call) append(out, sizeof out, "%s: err=%d", row, (int)(call))

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
    onload_out[0] = '\0';
    ROW(onload_out, "onload enable SingleStep without capability",
        (*jvmti)->SetEventNotificationMode(jvmti, JVMTI_ENABLE, JVMTI_EVENT_SINGLE_STEP, NULL));
    ROW(onload_out, "onload mode 7",
        (*jvmti)->SetEventNotificationMode(jvmti, (jvmtiEventMode)7, JVMTI_EVENT_THREAD_START, NULL));
    ROW(onload_out, "onload event 49",
        (*jvmti)->SetEventNotificationMode(jvmti, JVMTI_ENABLE, (jvmtiEvent)49, NULL));
    ROW(onload_out, "onload enable ThreadStart",
        (*jvmti)->SetEventNotificationMode(jvmti, JVMTI_ENABLE, JVMTI_EVENT_THREAD_START, NULL));
    ROW(onload_out, "onload disable ThreadStart",
        (*jvmti)->SetEventNotificationMode(jvmti, JVMTI_DISABLE, JVMTI_EVENT_THREAD_START, NULL));
    memset(&wanted, 0, sizeof wanted);
    wanted.can_generate_breakpoint_events = 1;
    ROW(onload_out, "onload AddCapabilities", (*jvmti)->AddCapabilities(jvmti, &wanted));
    ROW(onload_out, "onload enable Breakpoint",
        (*jvmti)->SetEventNotificationMode(jvmti, JVMTI_ENABLE, JVMTI_EVENT_BREAKPOINT, NULL));
    ROW(onload_out, "onload disable Breakpoint",
        (*jvmti)->SetEventNotificationMode(jvmti, JVMTI_DISABLE, JVMTI_EVENT_BREAKPOINT, NULL));
    agent_env = jvmti;
    return 0;
}

#define NO_ENV "no JVMTI env (load the shim with -agentpath too)\n"

JNIEXPORT jstring JNICALL
Java_L1W61JvmtiEventAndBreakpointErrors_onload(JNIEnv *env, jclass self)
{
    (void)self;
    return (*env)->NewStringUTF(env, agent_env == NULL ? NO_ENV : onload_out);
}

JNIEXPORT jint JNICALL
Java_L1W61JvmtiEventAndBreakpointErrors_nativeTarget(JNIEnv *env, jclass self)
{
    (void)env;
    (void)self;
    return 1;
}

JNIEXPORT jstring JNICALL
Java_L1W61JvmtiEventAndBreakpointErrors_rows(JNIEnv *env, jclass self, jthread unstarted)
{
    JavaVM *vm = NULL;
    jvmtiEnv *jvmti = agent_env;
    jvmtiEnv *bare = NULL;
    jthread current = NULL;
    jmethodID target, native_target, abstract_sides;
    jclass shape;
    jlocation start = 0, end = 0;
    char out[4096];
    (void)self;
    if (jvmti == NULL || (*env)->GetJavaVM(env, &vm) != JNI_OK
        || (*vm)->GetEnv(vm, (void **)&bare, JVMTI_VERSION_1_2) != JNI_OK) {
        return (*env)->NewStringUTF(env, NO_ENV);
    }
    out[0] = '\0';
    (*jvmti)->GetCurrentThread(jvmti, &current);
    target = (*env)->GetStaticMethodID(env, self, "target", "(I)I");
    native_target = (*env)->GetStaticMethodID(env, self, "nativeTarget", "()I");
    shape = (*env)->FindClass(env, "L1W61JvmtiEventAndBreakpointErrors$Shape");
    abstract_sides = (*env)->GetMethodID(env, shape, "sides", "()I");
    ROW(out, "mode 7", (*jvmti)->SetEventNotificationMode(jvmti, (jvmtiEventMode)7, JVMTI_EVENT_BREAKPOINT, NULL));
    ROW(out, "event 49", (*jvmti)->SetEventNotificationMode(jvmti, JVMTI_ENABLE, (jvmtiEvent)49, NULL));
    ROW(out, "event 89", (*jvmti)->SetEventNotificationMode(jvmti, JVMTI_ENABLE, (jvmtiEvent)89, NULL));
    ROW(out, "event 72", (*jvmti)->SetEventNotificationMode(jvmti, JVMTI_ENABLE, (jvmtiEvent)72, NULL));
    ROW(out, "event 85", (*jvmti)->SetEventNotificationMode(jvmti, JVMTI_ENABLE, (jvmtiEvent)85, NULL));
    ROW(out, "VMInit for a thread",
        (*jvmti)->SetEventNotificationMode(jvmti, JVMTI_ENABLE, JVMTI_EVENT_VM_INIT, current));
    ROW(out, "Breakpoint for a class",
        (*jvmti)->SetEventNotificationMode(jvmti, JVMTI_ENABLE, JVMTI_EVENT_BREAKPOINT, (jthread)self));
    ROW(out, "Breakpoint for an unstarted thread",
        (*jvmti)->SetEventNotificationMode(jvmti, JVMTI_ENABLE, JVMTI_EVENT_BREAKPOINT, unstarted));
    ROW(out, "SingleStep without capability",
        (*jvmti)->SetEventNotificationMode(jvmti, JVMTI_ENABLE, JVMTI_EVENT_SINGLE_STEP, NULL));
    ROW(out, "disable Breakpoint never enabled",
        (*jvmti)->SetEventNotificationMode(jvmti, JVMTI_DISABLE, JVMTI_EVENT_BREAKPOINT, NULL));
    ROW(out, "enable Breakpoint for this thread",
        (*jvmti)->SetEventNotificationMode(jvmti, JVMTI_ENABLE, JVMTI_EVENT_BREAKPOINT, current));
    ROW(out, "disable Breakpoint for this thread",
        (*jvmti)->SetEventNotificationMode(jvmti, JVMTI_DISABLE, JVMTI_EVENT_BREAKPOINT, current));
    ROW(out, "no capability enable Breakpoint",
        (*bare)->SetEventNotificationMode(bare, JVMTI_ENABLE, JVMTI_EVENT_BREAKPOINT, NULL));
    (*jvmti)->GetMethodLocation(jvmti, target, &start, &end);
    append(out, sizeof out, "target locations: %d..%d", (int)start, (int)end);
    ROW(out, "SetBreakpoint NULL method", (*jvmti)->SetBreakpoint(jvmti, NULL, 0));
    ROW(out, "SetBreakpoint location -1", (*jvmti)->SetBreakpoint(jvmti, target, -1));
    ROW(out, "SetBreakpoint past the end", (*jvmti)->SetBreakpoint(jvmti, target, end + 1));
    ROW(out, "SetBreakpoint 1000", (*jvmti)->SetBreakpoint(jvmti, target, 1000));
    ROW(out, "SetBreakpoint native", (*jvmti)->SetBreakpoint(jvmti, native_target, 0));
    ROW(out, "SetBreakpoint abstract", (*jvmti)->SetBreakpoint(jvmti, abstract_sides, 0));
    ROW(out, "SetBreakpoint 0", (*jvmti)->SetBreakpoint(jvmti, target, 0));
    ROW(out, "SetBreakpoint 0 again", (*jvmti)->SetBreakpoint(jvmti, target, 0));
    ROW(out, "SetBreakpoint end", (*jvmti)->SetBreakpoint(jvmti, target, end));
    ROW(out, "ClearBreakpoint 0", (*jvmti)->ClearBreakpoint(jvmti, target, 0));
    ROW(out, "ClearBreakpoint 0 again", (*jvmti)->ClearBreakpoint(jvmti, target, 0));
    ROW(out, "ClearBreakpoint end", (*jvmti)->ClearBreakpoint(jvmti, target, end));
    ROW(out, "ClearBreakpoint NULL method", (*jvmti)->ClearBreakpoint(jvmti, NULL, 0));
    ROW(out, "ClearBreakpoint location -1", (*jvmti)->ClearBreakpoint(jvmti, target, -1));
    ROW(out, "ClearBreakpoint native", (*jvmti)->ClearBreakpoint(jvmti, native_target, 0));
    ROW(out, "no capability SetBreakpoint", (*bare)->SetBreakpoint(bare, target, 0));
    ROW(out, "no capability ClearBreakpoint", (*bare)->ClearBreakpoint(bare, target, 0));
    (*bare)->DisposeEnvironment(bare);
    return (*env)->NewStringUTF(env, out);
}
