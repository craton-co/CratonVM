/*
 * Native shim and agent for tools/probes/interp/L1/L1W44JvmtiLocalsAndStack.java
 * (interpreter round i1 wave 44, lane L1). Loaded twice: as an agent
 * (`-agentpath`), whose `Agent_OnLoad` adds the three capabilities the
 * local-variable and monitor functions need (HotSpot grants them only in
 * the OnLoad phase, or in the live phase once a startup agent acquired
 * them), and with `System.load`, for the native `rows(lock, other)`, which
 * calls the JVMTI local-variable, frame and monitor functions on its own
 * thread and answers one line per call, `<row>: <answer>` (`err=<n>` for an
 * error). The rows that need no capability to fail use a second env,
 * obtained in the live phase, which has none.
 * `caller`'s slots, from its LocalVariableTable: i=0, l=1-2, f=3, d=4-5,
 * o=6, other=7, after=8, r=9 (not assigned yet at the call); slot 10 is
 * javac's hidden copy of the lock (no table entry). Build instructions are
 * in the probe's header.
 */
#include <jni.h>
#include <jvmti.h>
#include <stdarg.h>
#include <stdio.h>
#include <string.h>

static char out[8192];
static jvmtiEnv *agent_env = NULL;
static jvmtiCapabilities onload_potential;
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
    memset(&onload_potential, 0, sizeof onload_potential);
    (*jvmti)->GetPotentialCapabilities(jvmti, &onload_potential);
    memset(&wanted, 0, sizeof wanted);
    wanted.can_access_local_variables = 1;
    wanted.can_get_current_contended_monitor = 1;
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

/* The method name of `method`, or "?". */
static const char *name_of(jvmtiEnv *jvmti, jmethodID method, char *buf, size_t size)
{
    char *name = NULL;
    if ((*jvmti)->GetMethodName(jvmti, method, &name, NULL, NULL) == JVMTI_ERROR_NONE && name != NULL) {
        snprintf(buf, size, "%s", name);
        (*jvmti)->Deallocate(jvmti, (unsigned char *)name);
    } else {
        snprintf(buf, size, "?");
    }
    return buf;
}

/* `GetStackTrace(NULL, start, max)` as `err=<n>` or `n=<count> <names>`. */
static void trace_row(jvmtiEnv *jvmti, const char *row, jint start, jint max)
{
    jvmtiFrameInfo frames[32];
    jint n = -1;
    char names[400] = "";
    char buf[128];
    jint k;
    jvmtiError err = (*jvmti)->GetStackTrace(jvmti, NULL, start, max, frames, &n);
    if (err != JVMTI_ERROR_NONE) {
        line("%s: err=%d", row, (int)err);
        return;
    }
    for (k = 0; k < n && k < 32; k++) {
        strcat(names, " ");
        strcat(names, name_of(jvmti, frames[k].method, buf, sizeof buf));
    }
    line("%s: n=%d%s", row, (int)n, names);
}

/* An object's `toString()`, or "null". */
static void to_string(JNIEnv *env, jobject obj, char *buf, size_t size)
{
    jclass cls;
    jmethodID ts;
    jstring s;
    const char *chars;
    if (obj == NULL) {
        snprintf(buf, size, "null");
        return;
    }
    cls = (*env)->GetObjectClass(env, obj);
    ts = (*env)->GetMethodID(env, cls, "toString", "()Ljava/lang/String;");
    s = (jstring)(*env)->CallObjectMethod(env, obj, ts);
    chars = s == NULL ? NULL : (*env)->GetStringUTFChars(env, s, NULL);
    snprintf(buf, size, "%s", chars == NULL ? "?" : chars);
    if (chars != NULL) {
        (*env)->ReleaseStringUTFChars(env, s, chars);
    }
}

#define ERR_ROW(row, call) line("%s: err=%d", row, (int)(call))

JNIEXPORT jstring JNICALL
Java_L1W44JvmtiLocalsAndStack_rows(JNIEnv *env, jclass self, jobject lock, jthread other)
{
    JavaVM *vm = NULL;
    jvmtiEnv *jvmti = NULL;
    jvmtiCapabilities potential;
    jvmtiCapabilities wanted;
    jvmtiError err;
    jint count = -1;
    jint iv = -1;
    jlong jv = -1;
    jfloat fv = -1;
    jdouble dv = -1;
    jobject ov = NULL;
    jmethodID method = NULL;
    jlocation location = -2;
    jthread current = NULL;
    jobject monitor = NULL;
    jint depth_count = -1;
    jvmtiMonitorStackDepthInfo *depth_info = NULL;
    char buf[128];
    char text[128];
    (void)self;
    out[0] = '\0';
    if ((*env)->GetJavaVM(env, &vm) != JNI_OK
        || (*vm)->GetEnv(vm, (void **)&jvmti, JVMTI_VERSION_1_2) != JNI_OK) {
        return (*env)->NewStringUTF(env, "no JVMTI env\n");
    }

    memset(&potential, 0, sizeof potential);
    err = (*jvmti)->GetPotentialCapabilities(jvmti, &potential);
    line("live-phase potential: err=%d access_local_variables=%d current_contended_monitor=%d owned_monitor_stack_depth_info=%d",
         (int)err, potential.can_access_local_variables, potential.can_get_current_contended_monitor,
         potential.can_get_owned_monitor_stack_depth_info);

    /* Without the capabilities. */
    ERR_ROW("no capability GetLocalInt", (*jvmti)->GetLocalInt(jvmti, NULL, 1, 0, &iv));
    ERR_ROW("no capability SetLocalInt", (*jvmti)->SetLocalInt(jvmti, NULL, 1, 0, 5));
    ERR_ROW("no capability GetLocalInstance", (*jvmti)->GetLocalInstance(jvmti, NULL, 1, &ov));
    ERR_ROW("no capability GetCurrentContendedMonitor", (*jvmti)->GetCurrentContendedMonitor(jvmti, NULL, &monitor));
    ERR_ROW("no capability GetOwnedMonitorStackDepthInfo",
            (*jvmti)->GetOwnedMonitorStackDepthInfo(jvmti, NULL, &depth_count, &depth_info));

    memset(&wanted, 0, sizeof wanted);
    wanted.can_access_local_variables = 1;
    wanted.can_get_current_contended_monitor = 1;
    wanted.can_get_owned_monitor_stack_depth_info = 1;
    ERR_ROW("AddCapabilities in the live phase", (*jvmti)->AddCapabilities(jvmti, &wanted));
    line("Agent_OnLoad potential: access_local_variables=%d current_contended_monitor=%d owned_monitor_stack_depth_info=%d",
         onload_potential.can_access_local_variables, onload_potential.can_get_current_contended_monitor,
         onload_potential.can_get_owned_monitor_stack_depth_info);
    line("Agent_OnLoad AddCapabilities: err=%d", onload_add);
    if (agent_env == NULL) {
        line("not loaded as an agent");
        return (*env)->NewStringUTF(env, out);
    }
    jvmti = agent_env;

    /* Frames. */
    err = (*jvmti)->GetFrameCount(jvmti, NULL, &count);
    line("GetFrameCount: err=%d count=%d", (int)err, (int)count);
    err = (*jvmti)->GetFrameLocation(jvmti, NULL, 0, &method, &location);
    line("GetFrameLocation 0: err=%d %s@%d", (int)err, err ? "-" : name_of(jvmti, method, buf, sizeof buf), (int)location);
    location = -2;
    err = (*jvmti)->GetFrameLocation(jvmti, NULL, 1, &method, &location);
    line("GetFrameLocation 1: err=%d %s@%d", (int)err, err ? "-" : name_of(jvmti, method, buf, sizeof buf), (int)location);
    ERR_ROW("GetFrameLocation -1", (*jvmti)->GetFrameLocation(jvmti, NULL, -1, &method, &location));
    ERR_ROW("GetFrameLocation count", (*jvmti)->GetFrameLocation(jvmti, NULL, count, &method, &location));
    ERR_ROW("GetFrameLocation NULL method_ptr", (*jvmti)->GetFrameLocation(jvmti, NULL, 1, NULL, &location));
    trace_row(jvmti, "GetStackTrace -1 5", -1, 5);
    trace_row(jvmti, "GetStackTrace -2 1", -2, 1);
    trace_row(jvmti, "GetStackTrace -count 32", -count, 32);
    trace_row(jvmti, "GetStackTrace -(count+1) 32", -(count + 1), 32);
    trace_row(jvmti, "GetStackTrace count 32", count, 32);
    trace_row(jvmti, "GetStackTrace count-1 32", count - 1, 32);
    trace_row(jvmti, "GetStackTrace 0 0", 0, 0);
    trace_row(jvmti, "GetStackTrace 1 2", 1, 2);
    trace_row(jvmti, "GetStackTrace 0 -1", 0, -1);

    /* Locals of `caller` (depth 1), each of its kind. */
    iv = -1;
    err = (*jvmti)->GetLocalInt(jvmti, NULL, 1, 0, &iv);
    line("GetLocalInt i: err=%d %d", (int)err, (int)iv);
    err = (*jvmti)->GetLocalLong(jvmti, NULL, 1, 1, &jv);
    line("GetLocalLong l: err=%d %lld", (int)err, (long long)jv);
    err = (*jvmti)->GetLocalFloat(jvmti, NULL, 1, 3, &fv);
    line("GetLocalFloat f: err=%d %g", (int)err, (double)fv);
    err = (*jvmti)->GetLocalDouble(jvmti, NULL, 1, 4, &dv);
    line("GetLocalDouble d: err=%d %g", (int)err, dv);
    ov = NULL;
    err = (*jvmti)->GetLocalObject(jvmti, NULL, 1, 6, &ov);
    to_string(env, ov, text, sizeof text);
    line("GetLocalObject o: err=%d %s", (int)err, err ? "-" : text);
    iv = -1;
    err = (*jvmti)->GetLocalInt(jvmti, NULL, 1, 8, &iv);
    line("GetLocalInt after: err=%d %d", (int)err, (int)iv);
    ov = NULL;
    err = (*jvmti)->GetLocalObject(jvmti, NULL, 1, 7, &ov);
    line("GetLocalObject other: err=%d same=%d", (int)err, err ? -1 : (int)(*env)->IsSameObject(env, ov, other));
    err = (*jvmti)->GetCurrentThread(jvmti, &current);
    iv = -1;
    if (err == JVMTI_ERROR_NONE) {
        err = (*jvmti)->GetLocalInt(jvmti, current, 1, 0, &iv);
    }
    line("GetLocalInt i of the current thread's jthread: err=%d %d", (int)err, (int)iv);

    /* The kind or the slot does not fit. */
    ERR_ROW("GetLocalInt l", (*jvmti)->GetLocalInt(jvmti, NULL, 1, 1, &iv));
    ERR_ROW("GetLocalInt second half of l", (*jvmti)->GetLocalInt(jvmti, NULL, 1, 2, &iv));
    ERR_ROW("GetLocalInt f", (*jvmti)->GetLocalInt(jvmti, NULL, 1, 3, &iv));
    ERR_ROW("GetLocalInt o", (*jvmti)->GetLocalInt(jvmti, NULL, 1, 6, &iv));
    ERR_ROW("GetLocalLong i", (*jvmti)->GetLocalLong(jvmti, NULL, 1, 0, &jv));
    ERR_ROW("GetLocalLong d", (*jvmti)->GetLocalLong(jvmti, NULL, 1, 4, &jv));
    ERR_ROW("GetLocalFloat d", (*jvmti)->GetLocalFloat(jvmti, NULL, 1, 4, &fv));
    ERR_ROW("GetLocalDouble f", (*jvmti)->GetLocalDouble(jvmti, NULL, 1, 3, &dv));
    ERR_ROW("GetLocalObject i", (*jvmti)->GetLocalObject(jvmti, NULL, 1, 0, &ov));
    ERR_ROW("GetLocalObject r (not assigned yet)", (*jvmti)->GetLocalObject(jvmti, NULL, 1, 9, &ov));
    ERR_ROW("GetLocalInt r (not assigned yet)", (*jvmti)->GetLocalInt(jvmti, NULL, 1, 9, &iv));
    ERR_ROW("GetLocalObject slot 10 (no table entry)", (*jvmti)->GetLocalObject(jvmti, NULL, 1, 10, &ov));
    ERR_ROW("GetLocalInt slot 11 (no table entry, never written)", (*jvmti)->GetLocalInt(jvmti, NULL, 1, 11, &iv));
    ERR_ROW("GetLocalDouble slot 11 (past max_locals)", (*jvmti)->GetLocalDouble(jvmti, NULL, 1, 11, &dv));
    ERR_ROW("GetLocalInt slot 99", (*jvmti)->GetLocalInt(jvmti, NULL, 1, 99, &iv));
    ERR_ROW("GetLocalInt slot -1", (*jvmti)->GetLocalInt(jvmti, NULL, 1, -1, &iv));

    /* The frame does not fit. */
    ERR_ROW("GetLocalInt depth 0 (the native)", (*jvmti)->GetLocalInt(jvmti, NULL, 0, 0, &iv));
    ERR_ROW("GetLocalInt depth -1", (*jvmti)->GetLocalInt(jvmti, NULL, -1, 0, &iv));
    ERR_ROW("GetLocalInt depth count", (*jvmti)->GetLocalInt(jvmti, NULL, count, 0, &iv));
    ERR_ROW("GetLocalInt NULL value_ptr", (*jvmti)->GetLocalInt(jvmti, NULL, 1, 0, NULL));
    ERR_ROW("GetLocalInstance depth 1 (static)", (*jvmti)->GetLocalInstance(jvmti, NULL, 1, &ov));
    ERR_ROW("GetLocalInstance depth 0 (the native)", (*jvmti)->GetLocalInstance(jvmti, NULL, 0, &ov));
    ERR_ROW("GetLocalInt of a running other thread", (*jvmti)->GetLocalInt(jvmti, other, 1, 0, &iv));
    ERR_ROW("GetFrameCount of a running other thread", (*jvmti)->GetFrameCount(jvmti, other, &iv));

    /* Writes. */
    ERR_ROW("SetLocalInt after=42", (*jvmti)->SetLocalInt(jvmti, NULL, 1, 8, 42));
    ERR_ROW("SetLocalInt i=9", (*jvmti)->SetLocalInt(jvmti, NULL, 1, 0, 9));
    ERR_ROW("SetLocalLong i", (*jvmti)->SetLocalLong(jvmti, NULL, 1, 0, 1));
    ERR_ROW("SetLocalFloat d", (*jvmti)->SetLocalFloat(jvmti, NULL, 1, 4, 1.0f));
    ERR_ROW("SetLocalObject i", (*jvmti)->SetLocalObject(jvmti, NULL, 1, 0, lock));
    ERR_ROW("SetLocalInt slot 99", (*jvmti)->SetLocalInt(jvmti, NULL, 1, 99, 1));
    ERR_ROW("SetLocalInt depth 0 (the native)", (*jvmti)->SetLocalInt(jvmti, NULL, 0, 0, 1));
    err = (*jvmti)->SetLocalObject(jvmti, NULL, 1, 6, lock);
    ov = NULL;
    (*jvmti)->GetLocalObject(jvmti, NULL, 1, 6, &ov);
    line("SetLocalObject o=lock: err=%d reads back the lock=%d", (int)err, (int)(*env)->IsSameObject(env, ov, lock));
    iv = -1;
    (*jvmti)->GetLocalInt(jvmti, NULL, 1, 8, &iv);
    line("GetLocalInt after, after the write: %d", (int)iv);

    /* Monitors. */
    monitor = lock;
    err = (*jvmti)->GetCurrentContendedMonitor(jvmti, NULL, &monitor);
    line("GetCurrentContendedMonitor: err=%d null=%d", (int)err, monitor == NULL);
    ERR_ROW("GetCurrentContendedMonitor NULL monitor_ptr", (*jvmti)->GetCurrentContendedMonitor(jvmti, NULL, NULL));
    err = (*jvmti)->GetOwnedMonitorStackDepthInfo(jvmti, NULL, &depth_count, &depth_info);
    if (err == JVMTI_ERROR_NONE) {
        jint k;
        char rows[256] = "";
        for (k = 0; k < depth_count; k++) {
            char item[64];
            snprintf(item, sizeof item, " depth=%d lock=%d", (int)depth_info[k].stack_depth,
                     (int)(*env)->IsSameObject(env, depth_info[k].monitor, lock));
            strcat(rows, item);
        }
        line("GetOwnedMonitorStackDepthInfo: err=0 count=%d%s", (int)depth_count, rows);
        (*jvmti)->Deallocate(jvmti, (unsigned char *)depth_info);
    } else {
        line("GetOwnedMonitorStackDepthInfo: err=%d", (int)err);
    }
    ERR_ROW("GetOwnedMonitorStackDepthInfo NULL count_ptr",
            (*jvmti)->GetOwnedMonitorStackDepthInfo(jvmti, NULL, NULL, &depth_info));
    ERR_ROW("GetCurrentContendedMonitor of a running other thread",
            (*jvmti)->GetCurrentContendedMonitor(jvmti, other, &monitor));
    ERR_ROW("GetOwnedMonitorStackDepthInfo of a running other thread",
            (*jvmti)->GetOwnedMonitorStackDepthInfo(jvmti, other, &depth_count, &depth_info));

    return (*env)->NewStringUTF(env, out);
}
