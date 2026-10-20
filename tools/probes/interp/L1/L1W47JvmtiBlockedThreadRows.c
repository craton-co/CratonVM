/*
 * Native shim and agent for tools/probes/interp/L1/L1W47JvmtiBlockedThreadRows.java
 * (interpreter round i1 wave 47, lane L1). Loaded twice: as an agent
 * (`-agentpath`), whose `Agent_OnLoad` adds `can_suspend` and
 * `can_access_local_variables`, and with `System.load`, for the natives
 * `rows(...)`, which suspends three threads blocked in `Object.wait`, lists
 * their stacks and reads and writes a local of one, answering one line per
 * call (`err=<n>` for an error), and `through(r)`, which calls `r.run()`
 * through JNI. Build instructions are in the probe's header.
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
    onload_add = (int)(*jvmti)->AddCapabilities(jvmti, &wanted);
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

/* The objects a local is compared with. */
static jobject known[2];
static const char *known_names[2] = { "obj", "written" };

static const char *object_name(JNIEnv *env, jobject obj)
{
    int k;
    if (obj == NULL) {
        return "null";
    }
    for (k = 0; k < 2; k++) {
        if ((*env)->IsSameObject(env, obj, known[k])) {
            return known_names[k];
        }
    }
    return "?";
}

/*
 * The simple name of `method`'s class into `simple` (`Object`,
 * `L1W47JvmtiBlockedThreadRows$Waiter`); answers whether the frame is left
 * out: a class of `java/lang/invoke`, a hidden class (its name holds
 * `/0x`), or a method named `invokeImpl`.
 */
static int frame_name(jvmtiEnv *jvmti, jmethodID method, char *simple, size_t size, char *name_out,
                      size_t name_size)
{
    jclass klass = NULL;
    char *signature = NULL;
    char *name = NULL;
    int skip = 0;
    simple[0] = '\0';
    name_out[0] = '\0';
    if ((*jvmti)->GetMethodName(jvmti, method, &name, NULL, NULL) == JVMTI_ERROR_NONE && name != NULL) {
        snprintf(name_out, name_size, "%s", name);
        skip = strcmp(name, "invokeImpl") == 0;
        (*jvmti)->Deallocate(jvmti, (unsigned char *)name);
    } else {
        snprintf(name_out, name_size, "?");
    }
    if ((*jvmti)->GetMethodDeclaringClass(jvmti, method, &klass) == JVMTI_ERROR_NONE
        && (*jvmti)->GetClassSignature(jvmti, klass, &signature, NULL) == JVMTI_ERROR_NONE
        && signature != NULL) {
        const char *start = strrchr(signature, '/');
        size_t len;
        if (strncmp(signature, "Ljava/lang/invoke/", 18) == 0 || strstr(signature, "/0x") != NULL) {
            skip = 1;
        }
        start = start != NULL ? start + 1 : signature + 1;
        len = strlen(start);
        if (len > 0 && start[len - 1] == ';') {
            len--;
        }
        if (len >= size) {
            len = size - 1;
        }
        memcpy(simple, start, len);
        simple[len] = '\0';
        (*jvmti)->Deallocate(jvmti, (unsigned char *)signature);
    } else {
        snprintf(simple, size, "?");
    }
    return skip;
}

/*
 * `GetStackTrace(thread, 0, 32)` as `err=<n>`, or the frames from the top
 * down to the first `run` of a class named `stop` (`Class.method@location`),
 * leaving out the frames `frame_name` leaves out.
 */
static void trace_row(jvmtiEnv *jvmti, const char *row, jthread thread, const char *stop)
{
    jvmtiFrameInfo frames[32];
    jint n = -1;
    char listed[900] = "";
    char item[200];
    jint k;
    jvmtiError err = (*jvmti)->GetStackTrace(jvmti, thread, 0, 32, frames, &n);
    if (err != JVMTI_ERROR_NONE) {
        line("%s: err=%d", row, (int)err);
        return;
    }
    for (k = 0; k < n && k < 32; k++) {
        char simple[128];
        char name[64];
        if (frame_name(jvmti, frames[k].method, simple, sizeof simple, name, sizeof name)) {
            continue;
        }
        snprintf(item, sizeof item, " %s.%s@%d", simple, name, (int)frames[k].location);
        if (strlen(listed) + strlen(item) + 1 < sizeof listed) {
            strcat(listed, item);
        }
        if (strcmp(name, "run") == 0 && strcmp(simple, stop) == 0) {
            break;
        }
    }
    line("%s:%s", row, listed);
}

/* The depth of the first `run` of class `stop` on `thread`'s stack, or -1. */
static jint run_depth(jvmtiEnv *jvmti, jthread thread, const char *stop)
{
    jvmtiFrameInfo frames[32];
    jint n = -1;
    jint k;
    if ((*jvmti)->GetStackTrace(jvmti, thread, 0, 32, frames, &n) != JVMTI_ERROR_NONE) {
        return -1;
    }
    for (k = 0; k < n && k < 32; k++) {
        char simple[128];
        char name[64];
        (void)frame_name(jvmti, frames[k].method, simple, sizeof simple, name, sizeof name);
        if (strcmp(name, "run") == 0 && strcmp(simple, stop) == 0) {
            return k;
        }
    }
    return -1;
}

static void object_row(JNIEnv *env, jvmtiEnv *jvmti, const char *row, jthread thread, jint depth, jint slot)
{
    jobject value = NULL;
    jvmtiError err = (*jvmti)->GetLocalObject(jvmti, thread, depth, slot, &value);
    if (err != JVMTI_ERROR_NONE) {
        line("%s: err=%d", row, (int)err);
        return;
    }
    line("%s: err=0 %s", row, object_name(env, value));
}

#define ERR_ROW(row, call) line("%s: err=%d", row, (int)(call))

/* `L1W47JvmtiBlockedThreadRows.through(r)`: `r.run()` through JNI. */
JNIEXPORT void JNICALL
Java_L1W47JvmtiBlockedThreadRows_through(JNIEnv *env, jclass self, jobject r)
{
    jclass runnable = (*env)->FindClass(env, "java/lang/Runnable");
    jmethodID run = (*env)->GetMethodID(env, runnable, "run", "()V");
    (void)self;
    (*env)->CallVoidMethod(env, r, run);
}

JNIEXPORT jstring JNICALL
Java_L1W47JvmtiBlockedThreadRows_rows(JNIEnv *env, jclass self, jthread waiter, jthread upcaller,
                                      jthread reflector, jstring obj, jstring written)
{
    jvmtiEnv *jvmti = agent_env;
    jint depth;
    (void)self;
    out[0] = '\0';
    if (jvmti == NULL) {
        return (*env)->NewStringUTF(env, "no JVMTI env (load the shim with -agentpath too)\n");
    }
    known[0] = obj;
    known[1] = written;
    line("Agent_OnLoad AddCapabilities: err=%d", onload_add);

    /* The waiter: its stack, and its local `o` (slot 1 of `Waiter.run`). */
    ERR_ROW("SuspendThread waiter", (*jvmti)->SuspendThread(jvmti, waiter));
    trace_row(jvmti, "GetStackTrace waiter", waiter, "L1W47JvmtiBlockedThreadRows$Waiter");
    depth = run_depth(jvmti, waiter, "L1W47JvmtiBlockedThreadRows$Waiter");
    object_row(env, jvmti, "GetLocalObject waiter o", waiter, depth, 1);
    ERR_ROW("SetLocalObject waiter o=written", (*jvmti)->SetLocalObject(jvmti, waiter, depth, 1, written));
    object_row(env, jvmti, "GetLocalObject waiter o after the write", waiter, depth, 1);
    ERR_ROW("ResumeThread waiter", (*jvmti)->ResumeThread(jvmti, waiter));

    /* The upcaller: a JNI native in the middle of its stack. */
    ERR_ROW("SuspendThread upcaller", (*jvmti)->SuspendThread(jvmti, upcaller));
    trace_row(jvmti, "GetStackTrace upcaller", upcaller, "L1W47JvmtiBlockedThreadRows$Upcaller");
    ERR_ROW("ResumeThread upcaller", (*jvmti)->ResumeThread(jvmti, upcaller));

    /* The reflector: a reflective call's JDK frames. */
    ERR_ROW("SuspendThread reflector", (*jvmti)->SuspendThread(jvmti, reflector));
    trace_row(jvmti, "GetStackTrace reflector", reflector, "L1W47JvmtiBlockedThreadRows$Reflector");
    ERR_ROW("ResumeThread reflector", (*jvmti)->ResumeThread(jvmti, reflector));
    return (*env)->NewStringUTF(env, out);
}
