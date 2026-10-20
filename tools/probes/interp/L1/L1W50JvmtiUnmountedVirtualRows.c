/*
 * Native shim and agent for tools/probes/interp/L1/L1W50JvmtiUnmountedVirtualRows.java
 * (interpreter round i1 wave 50, lane L1). Loaded twice: as an agent
 * (`-agentpath`), whose `Agent_OnLoad` keeps the JVMTI env (no capability
 * is needed), and with `System.load`, for the native `rows(...)`, which
 * prints `GetFrameCount` and `GetStackTrace` of two unmounted virtual
 * threads. Build instructions are in the probe's header.
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
    (void)options;
    (void)reserved;
    if ((*vm)->GetEnv(vm, (void **)&jvmti, JVMTI_VERSION_1_2) != JNI_OK) {
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

/* `Class.method` of `method`, the class by its simple name. */
static void frame_name(jvmtiEnv *jvmti, jmethodID method, char *dst, size_t size, char *name_out,
                       size_t name_size)
{
    jclass klass = NULL;
    char *signature = NULL;
    char *name = NULL;
    char simple[128] = "?";
    name_out[0] = '\0';
    if ((*jvmti)->GetMethodName(jvmti, method, &name, NULL, NULL) == JVMTI_ERROR_NONE && name != NULL) {
        snprintf(name_out, name_size, "%s", name);
        (*jvmti)->Deallocate(jvmti, (unsigned char *)name);
    } else {
        snprintf(name_out, name_size, "?");
    }
    if ((*jvmti)->GetMethodDeclaringClass(jvmti, method, &klass) == JVMTI_ERROR_NONE
        && (*jvmti)->GetClassSignature(jvmti, klass, &signature, NULL) == JVMTI_ERROR_NONE
        && signature != NULL) {
        const char *start = strrchr(signature, '/');
        size_t len;
        start = start != NULL ? start + 1 : signature + 1;
        len = strlen(start);
        if (len > 0 && start[len - 1] == ';') {
            len--;
        }
        if (len >= sizeof simple) {
            len = sizeof simple - 1;
        }
        memcpy(simple, start, len);
        simple[len] = '\0';
        (*jvmti)->Deallocate(jvmti, (unsigned char *)signature);
    }
    snprintf(dst, size, "%s.%s", simple, name_out);
}

static void thread_rows(jvmtiEnv *jvmti, const char *label, jthread thread, const char *stop)
{
    jvmtiFrameInfo frames[32];
    jint count = -1;
    jint n = -1;
    jint k;
    char listed[1200] = "";
    jvmtiError err = (*jvmti)->GetFrameCount(jvmti, thread, &count);
    line("GetFrameCount %s: err=%d", label, (int)err);
    err = (*jvmti)->GetStackTrace(jvmti, thread, 0, 32, frames, &n);
    if (err != JVMTI_ERROR_NONE) {
        line("GetStackTrace %s: err=%d", label, (int)err);
        return;
    }
    for (k = 0; k < n && k < 32; k++) {
        char item[256];
        char name[64];
        frame_name(jvmti, frames[k].method, item, sizeof item, name, sizeof name);
        if (strlen(listed) + strlen(item) + 2 < sizeof listed) {
            strcat(listed, " ");
            strcat(listed, item);
        }
        if (strcmp(name, stop) == 0) {
            break;
        }
    }
    line("GetStackTrace %s:%s", label, listed);
}

JNIEXPORT jstring JNICALL
Java_L1W50JvmtiUnmountedVirtualRows_rows(JNIEnv *env, jclass self, jthread sleeper, jthread parker)
{
    jvmtiEnv *jvmti = agent_env;
    (void)self;
    out[0] = '\0';
    if (jvmti == NULL) {
        return (*env)->NewStringUTF(env, "no JVMTI env (load the shim with -agentpath too)\n");
    }
    thread_rows(jvmti, "sleeper", sleeper, "sleepIn");
    thread_rows(jvmti, "parker", parker, "parkIn");
    return (*env)->NewStringUTF(env, out);
}
