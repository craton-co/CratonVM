/*
 * Native shim and agent for tools/probes/interp/L1/L1W46JvmtiOtherThreadLocalsAndMonitors.java
 * (interpreter round i1 wave 46, lane L1). Loaded twice: as an agent
 * (`-agentpath`), whose `Agent_OnLoad` adds `can_suspend`,
 * `can_access_local_variables` and `can_get_owned_monitor_stack_depth_info`,
 * and with `System.load`, for the native `rows(...)`, which calls the local
 * and monitor functions on two other threads and answers one line per call,
 * `<row>: <answer>` (`err=<n>` for an error). Build instructions are in the
 * probe's header.
 */
#include <jni.h>
#include <jvmti.h>
#include <stdarg.h>
#include <stdio.h>
#include <string.h>

static char out[8192];
static jvmtiEnv *agent_env = NULL;
static int onload_add = -1;

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
    onload_add = (int)(*jvmti)->AddCapabilities(jvmti, &wanted);
    agent_env = jvmti;
    return 0;
}

static void line(const char *fmt, ...)
{
    char item[512];
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

/* The objects a monitor or a local is compared with. */
static jobject known[6];
static const char *known_names[6] = { "M1", "M2", "W", "HC", "obj", "written" };

static const char *object_name(JNIEnv *env, jobject obj)
{
    int k;
    if (obj == NULL) {
        return "null";
    }
    for (k = 0; k < 6; k++) {
        if ((*env)->IsSameObject(env, obj, known[k])) {
            return known_names[k];
        }
    }
    return "?";
}

/* The method name at `depth` of `thread`, or `err=<n>`. */
static void frame_row(jvmtiEnv *jvmti, const char *row, jthread thread, jint depth)
{
    jmethodID method = NULL;
    jlocation location = -2;
    char *name = NULL;
    jvmtiError err = (*jvmti)->GetFrameLocation(jvmti, thread, depth, &method, &location);
    if (err != JVMTI_ERROR_NONE) {
        line("%s: err=%d", row, (int)err);
        return;
    }
    if ((*jvmti)->GetMethodName(jvmti, method, &name, NULL, NULL) != JVMTI_ERROR_NONE || name == NULL) {
        line("%s: err=0 ?", row);
        return;
    }
    line("%s: err=0 %s", row, name);
    (*jvmti)->Deallocate(jvmti, (unsigned char *)name);
}

/* `GetStackTrace(thread, 0, max)` as `err=<n>` or `n=<count> <names>`. */
static void trace_row(jvmtiEnv *jvmti, const char *row, jthread thread, jint max)
{
    jvmtiFrameInfo frames[16];
    jint n = -1;
    char names[400] = "";
    jint k;
    jvmtiError err = (*jvmti)->GetStackTrace(jvmti, thread, 0, max, frames, &n);
    if (err != JVMTI_ERROR_NONE) {
        line("%s: err=%d", row, (int)err);
        return;
    }
    for (k = 0; k < n && k < 16; k++) {
        char *name = NULL;
        strcat(names, " ");
        if ((*jvmti)->GetMethodName(jvmti, frames[k].method, &name, NULL, NULL) == JVMTI_ERROR_NONE
            && name != NULL) {
            strncat(names, name, 64);
            (*jvmti)->Deallocate(jvmti, (unsigned char *)name);
        } else {
            strcat(names, "?");
        }
    }
    line("%s: n=%d%s", row, (int)n, names);
}

/* `GetFrameLocation(thread, depth)` as `err=<n>` or `err=0 <name>@<location>`. */
static void location_row(jvmtiEnv *jvmti, const char *row, jthread thread, jint depth)
{
    jmethodID method = NULL;
    jlocation location = -2;
    char *name = NULL;
    jvmtiError err = (*jvmti)->GetFrameLocation(jvmti, thread, depth, &method, &location);
    if (err != JVMTI_ERROR_NONE) {
        line("%s: err=%d", row, (int)err);
        return;
    }
    if ((*jvmti)->GetMethodName(jvmti, method, &name, NULL, NULL) != JVMTI_ERROR_NONE || name == NULL) {
        line("%s: err=0 ?@%d", row, (int)location);
        return;
    }
    line("%s: err=0 %s@%d", row, name, (int)location);
    (*jvmti)->Deallocate(jvmti, (unsigned char *)name);
}

/* `L1W46JvmtiOtherThreadLocalsAndMonitors.through(r)`: `r.run()` through JNI. */
JNIEXPORT void JNICALL
Java_L1W46JvmtiOtherThreadLocalsAndMonitors_through(JNIEnv *env, jclass self, jobject r)
{
    jclass runnable = (*env)->FindClass(env, "java/lang/Runnable");
    jmethodID run = (*env)->GetMethodID(env, runnable, "run", "()V");
    (void)self;
    (*env)->CallVoidMethod(env, r, run);
}

/* `GetOwnedMonitorStackDepthInfo(thread)` as `err=<n>` or `err=0 count=<n> <name>@<depth>...`. */
static void monitors_row(JNIEnv *env, jvmtiEnv *jvmti, const char *row, jthread thread)
{
    jint count = -1;
    jvmtiMonitorStackDepthInfo *info = NULL;
    char list[256] = "";
    char item[64];
    jint k;
    jvmtiError err = (*jvmti)->GetOwnedMonitorStackDepthInfo(jvmti, thread, &count, &info);
    if (err != JVMTI_ERROR_NONE) {
        line("%s: err=%d", row, (int)err);
        return;
    }
    for (k = 0; k < count; k++) {
        snprintf(item, sizeof item, " %s@%d", object_name(env, info[k].monitor), (int)info[k].stack_depth);
        strcat(list, item);
    }
    line("%s: err=0 count=%d%s", row, (int)count, list);
    (*jvmti)->Deallocate(jvmti, (unsigned char *)info);
}

static void int_row(jvmtiEnv *jvmti, const char *row, jthread thread, jint depth, jint slot)
{
    jint value = -1;
    jvmtiError err = (*jvmti)->GetLocalInt(jvmti, thread, depth, slot, &value);
    if (err != JVMTI_ERROR_NONE) {
        line("%s: err=%d", row, (int)err);
        return;
    }
    line("%s: err=0 %d", row, (int)value);
}

static void long_row(jvmtiEnv *jvmti, const char *row, jthread thread, jint depth, jint slot)
{
    jlong value = -1;
    jvmtiError err = (*jvmti)->GetLocalLong(jvmti, thread, depth, slot, &value);
    if (err != JVMTI_ERROR_NONE) {
        line("%s: err=%d", row, (int)err);
        return;
    }
    line("%s: err=0 %lld", row, (long long)value);
}

static void object_row(JNIEnv *env, jvmtiEnv *jvmti, const char *row, jthread thread, jint depth,
                       jint slot, jobject same_as)
{
    jobject value = NULL;
    jvmtiError err = (*jvmti)->GetLocalObject(jvmti, thread, depth, slot, &value);
    if (err != JVMTI_ERROR_NONE) {
        line("%s: err=%d", row, (int)err);
        return;
    }
    if (same_as != NULL) {
        line("%s: err=0 same=%d", row, (int)(*env)->IsSameObject(env, value, same_as));
    } else {
        line("%s: err=0 %s", row, object_name(env, value));
    }
}

#define ERR_ROW(row, call) line("%s: err=%d", row, (int)(call))

JNIEXPORT jstring JNICALL
Java_L1W46JvmtiOtherThreadLocalsAndMonitors_rows(JNIEnv *env, jclass self, jthread holder,
                                                 jthread waiter, jthread upcaller,
                                                 jobject m1, jobject m2,
                                                 jobject w, jobject holder_class, jstring obj,
                                                 jstring written)
{
    jvmtiEnv *jvmti = agent_env;
    jobject instance = NULL;
    (void)self;
    out[0] = '\0';
    if (jvmti == NULL) {
        return (*env)->NewStringUTF(env, "no JVMTI env (load the shim with -agentpath too)\n");
    }
    known[0] = m1;
    known[1] = m2;
    known[2] = w;
    known[3] = holder_class;
    known[4] = obj;
    known[5] = written;
    line("Agent_OnLoad AddCapabilities: err=%d", onload_add);

    /* The holder, running. */
    monitors_row(env, jvmti, "GetOwnedMonitorStackDepthInfo holder (running)", holder);
    int_row(jvmti, "GetLocalInt holder a (running)", holder, 0, 0);
    /* The waiter, waiting and not suspended. */
    monitors_row(env, jvmti, "GetOwnedMonitorStackDepthInfo waiter (waiting)", waiter);
    int_row(jvmti, "GetLocalInt waiter token (waiting, not suspended)", waiter, 3, 1);

    /* The holder, suspended. */
    ERR_ROW("SuspendThread holder", (*jvmti)->SuspendThread(jvmti, holder));
    frame_row(jvmti, "GetFrameLocation holder 0", holder, 0);
    frame_row(jvmti, "GetFrameLocation holder 1", holder, 1);
    int_row(jvmti, "GetLocalInt holder a", holder, 0, 0);
    long_row(jvmti, "GetLocalLong holder b", holder, 0, 1);
    object_row(env, jvmti, "GetLocalObject holder o", holder, 0, 3, NULL);
    int_row(jvmti, "GetLocalInt holder mark", holder, 0, 4);
    int_row(jvmti, "GetLocalInt holder b (a long)", holder, 0, 1);
    int_row(jvmti, "GetLocalInt holder slot 99", holder, 0, 99);
    int_row(jvmti, "GetLocalInt holder depth 1 slot 0 (outer has no locals)", holder, 1, 0);
    int_row(jvmti, "GetLocalInt holder depth 9", holder, 9, 0);
    ERR_ROW("GetLocalInstance holder depth 0 (static)",
            (*jvmti)->GetLocalInstance(jvmti, holder, 0, &instance));
    monitors_row(env, jvmti, "GetOwnedMonitorStackDepthInfo holder (suspended)", holder);
    ERR_ROW("SetLocalInt holder mark=8", (*jvmti)->SetLocalInt(jvmti, holder, 0, 4, 8));
    int_row(jvmti, "GetLocalInt holder mark after the write", holder, 0, 4);
    ERR_ROW("SetLocalObject holder o=written", (*jvmti)->SetLocalObject(jvmti, holder, 0, 3, written));
    object_row(env, jvmti, "GetLocalObject holder o after the write", holder, 0, 3, NULL);
    ERR_ROW("SetLocalLong holder a (an int)", (*jvmti)->SetLocalLong(jvmti, holder, 0, 0, 1));
    ERR_ROW("ResumeThread holder", (*jvmti)->ResumeThread(jvmti, holder));

    /* The waiter, suspended while it waits. */
    ERR_ROW("SuspendThread waiter", (*jvmti)->SuspendThread(jvmti, waiter));
    frame_row(jvmti, "GetFrameLocation waiter 0", waiter, 0);
    frame_row(jvmti, "GetFrameLocation waiter 3", waiter, 3);
    int_row(jvmti, "GetLocalInt waiter token", waiter, 3, 1);
    object_row(env, jvmti, "GetLocalObject waiter this", waiter, 3, 0, waiter);
    ERR_ROW("GetLocalInstance waiter depth 3",
            (*jvmti)->GetLocalInstance(jvmti, waiter, 3, &instance));
    line("GetLocalInstance waiter depth 3 is the waiter: %d",
         (int)(instance != NULL && (*env)->IsSameObject(env, instance, waiter)));
    int_row(jvmti, "GetLocalInt waiter depth 0 (the native)", waiter, 0, 0);
    int_row(jvmti, "GetLocalInt waiter slot 0 (this)", waiter, 3, 0);
    ERR_ROW("SetLocalInt waiter token=6", (*jvmti)->SetLocalInt(jvmti, waiter, 3, 1, 6));
    int_row(jvmti, "GetLocalInt waiter token after the write", waiter, 3, 1);
    monitors_row(env, jvmti, "GetOwnedMonitorStackDepthInfo waiter (suspended)", waiter);
    ERR_ROW("ResumeThread waiter", (*jvmti)->ResumeThread(jvmti, waiter));
    int_row(jvmti, "GetLocalInt waiter token (resumed)", waiter, 3, 1);

    /* The upcaller: a JNI native in the middle of its stack. */
    ERR_ROW("SuspendThread upcaller", (*jvmti)->SuspendThread(jvmti, upcaller));
    trace_row(jvmti, "GetStackTrace upcaller (suspended) 0 4", upcaller, 4);
    location_row(jvmti, "GetFrameLocation upcaller 2", upcaller, 2);
    location_row(jvmti, "GetFrameLocation upcaller 3", upcaller, 3);
    ERR_ROW("ResumeThread upcaller", (*jvmti)->ResumeThread(jvmti, upcaller));
    trace_row(jvmti, "GetStackTrace upcaller (running) 0 4", upcaller, 4);
    return (*env)->NewStringUTF(env, out);
}
