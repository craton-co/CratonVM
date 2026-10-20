/*
 * Native shim and agent for tools/probes/interp/L1/L1W132JvmtiVirtualThreadThis.java
 * (interpreter round i6 wave 132, lane L1; i119-L1 residual). Loaded twice: as
 * an agent (`-agentpath`), whose `Agent_OnLoad` adds `can_suspend` and
 * `can_access_local_variables`, and with `System.load`, for the native
 * `rows(...)`, which suspends an unmounted virtual thread and prints, for each
 * frame down to the probe's own method, the method and its GetLocalInstance
 * answer. Build instructions are in the probe's header.
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
    if ((*jvmti)->AddCapabilities(jvmti, &wanted) != JVMTI_ERROR_NONE) {
        return 1;
    }
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

/* `Lpkg/Name;` -> `Name` (the simple part after the last '/'), into `buf`. */
static void simple_name(const char *sig, char *buf, size_t size)
{
    const char *start = strrchr(sig, '/');
    size_t n;
    if (start != NULL) {
        start++;
    } else {
        start = sig[0] == 'L' ? sig + 1 : sig;
    }
    n = strlen(start);
    if (n > 0 && start[n - 1] == ';') {
        n--;
    }
    if (n >= size) {
        n = size - 1;
    }
    memcpy(buf, start, n);
    buf[n] = '\0';
}

static void thread_rows(JNIEnv *env, jvmtiEnv *jvmti, const char *label, jthread thread, const char *stop)
{
    jvmtiFrameInfo frames[32];
    jint n = -1;
    jint k;
    jvmtiError err = (*jvmti)->SuspendThread(jvmti, thread);
    line("== %s SuspendThread: err=%d", label, (int)err);
    err = (*jvmti)->GetStackTrace(jvmti, thread, 0, 32, frames, &n);
    if (err != JVMTI_ERROR_NONE) {
        line("  GetStackTrace: err=%d", (int)err);
        n = 0;
    }
    for (k = 0; k < n && k < 32; k++) {
        char *name = NULL;
        char *sig = NULL;
        char *class_sig = NULL;
        char simple[256];
        jclass klass = NULL;
        jobject oval = NULL;
        jboolean is_native = JNI_FALSE;
        int last = 0;
        char answer[64];
        (*jvmti)->GetMethodName(jvmti, frames[k].method, &name, &sig, NULL);
        (*jvmti)->GetMethodDeclaringClass(jvmti, frames[k].method, &klass);
        if (klass != NULL) {
            (*jvmti)->GetClassSignature(jvmti, klass, &class_sig, NULL);
        }
        (*jvmti)->IsMethodNative(jvmti, frames[k].method, &is_native);
        simple_name(class_sig != NULL ? class_sig : "?", simple, sizeof simple);
        err = (*jvmti)->GetLocalInstance(jvmti, thread, k, &oval);
        if (err != JVMTI_ERROR_NONE) {
            snprintf(answer, sizeof answer, "error %d", (int)err);
        } else if (oval == NULL) {
            snprintf(answer, sizeof answer, "null");
        } else if ((*env)->IsSameObject(env, oval, thread)) {
            snprintf(answer, sizeof answer, "self");
        } else {
            snprintf(answer, sizeof answer, "other");
        }
        line("  frame %d: %s.%s%s native=%s this=%s", (int)k, simple, name != NULL ? name : "?",
             sig != NULL ? sig : "?", is_native ? "true" : "false", answer);
        last = name != NULL && strcmp(name, stop) == 0;
        if (name != NULL) {
            (*jvmti)->Deallocate(jvmti, (unsigned char *)name);
        }
        if (sig != NULL) {
            (*jvmti)->Deallocate(jvmti, (unsigned char *)sig);
        }
        if (class_sig != NULL) {
            (*jvmti)->Deallocate(jvmti, (unsigned char *)class_sig);
        }
        if (oval != NULL) {
            (*env)->DeleteLocalRef(env, oval);
        }
        if (klass != NULL) {
            (*env)->DeleteLocalRef(env, klass);
        }
        if (last) {
            break;
        }
    }
    err = (*jvmti)->ResumeThread(jvmti, thread);
    line("  ResumeThread: err=%d", (int)err);
}

JNIEXPORT jstring JNICALL
Java_L1W132JvmtiVirtualThreadThis_rows(JNIEnv *env, jclass self, jthread sleeper, jthread parker)
{
    jvmtiEnv *jvmti = agent_env;
    (void)self;
    out[0] = '\0';
    if (jvmti == NULL) {
        return (*env)->NewStringUTF(env, "no JVMTI env (load the shim with -agentpath too)\n");
    }
    thread_rows(env, jvmti, "vsleeper", sleeper, "sleepIn");
    thread_rows(env, jvmti, "vparker", parker, "parkIn");
    return (*env)->NewStringUTF(env, out);
}
