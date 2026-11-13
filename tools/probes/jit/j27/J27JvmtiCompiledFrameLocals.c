/*
 * Native shim and agent for tools/probes/jit/j27/J27JvmtiCompiledFrameLocals.java
 * (j27 wave 5, lane JVMTIFRAME). Loaded twice: as an agent (`-agentpath`),
 * whose `Agent_OnLoad` adds can_access_local_variables and can_get_bytecodes
 * (an OnLoad grant is what makes this VM's compiles keep their locals for a
 * debugger), and with `System.load`, for the native
 * `rows(self, s, record)`, which, when `record`, reads its two Java callers
 * through the JVMTI frame and local-variable functions -- depth 1 `mid`
 * (compiled by then), depth 2 `main` (its loop OSR'd by then) -- and answers
 * one `fact <row> <answer>` line per call (`err=<n>` for an error).
 * Build instructions are in the probe's header.
 */
#include <jni.h>
#include <jvmti.h>
#include <stdarg.h>
#include <stdio.h>
#include <string.h>

static char out[4096];
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
    wanted.can_access_local_variables = 1;
    wanted.can_get_bytecodes = 1;
    if ((*jvmti)->AddCapabilities(jvmti, &wanted) != JVMTI_ERROR_NONE) {
        return 1;
    }
    agent_env = jvmti;
    return 0;
}

static void line(const char *fmt, ...)
{
    char item[256];
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

static void int_row(const char *row, jint depth, jint slot)
{
    jint v = 0;
    jvmtiError err = (*agent_env)->GetLocalInt(agent_env, NULL, depth, slot, &v);
    if (err != JVMTI_ERROR_NONE) {
        line("fact %s err=%d", row, (int)err);
    } else {
        line("fact %s %d", row, (int)v);
    }
}

static void long_row(const char *row, jint depth, jint slot)
{
    jlong v = 0;
    jvmtiError err = (*agent_env)->GetLocalLong(agent_env, NULL, depth, slot, &v);
    if (err != JVMTI_ERROR_NONE) {
        line("fact %s err=%d", row, (int)err);
    } else {
        line("fact %s %lld", row, (long long)v);
    }
}

static void double_row(const char *row, jint depth, jint slot)
{
    jdouble v = 0;
    jvmtiError err = (*agent_env)->GetLocalDouble(agent_env, NULL, depth, slot, &v);
    if (err != JVMTI_ERROR_NONE) {
        line("fact %s err=%d", row, (int)err);
    } else {
        line("fact %s %.3f", row, (double)v);
    }
}

/* `same` when the local is `expected`, `other` / `null` otherwise. */
static void object_row(JNIEnv *env, const char *row, jint depth, jint slot, jobject expected)
{
    jobject v = NULL;
    jvmtiError err = (*agent_env)->GetLocalObject(agent_env, NULL, depth, slot, &v);
    if (err != JVMTI_ERROR_NONE) {
        line("fact %s err=%d", row, (int)err);
        return;
    }
    line("fact %s %s", row, v == NULL ? "null" : ((*env)->IsSameObject(env, v, expected) ? "same" : "other"));
}

static void instance_row(JNIEnv *env, const char *row, jint depth, jobject expected)
{
    jobject v = NULL;
    jvmtiError err = (*agent_env)->GetLocalInstance(agent_env, NULL, depth, &v);
    if (err != JVMTI_ERROR_NONE) {
        line("fact %s err=%d", row, (int)err);
        return;
    }
    line("fact %s %s", row, v == NULL ? "null" : ((*env)->IsSameObject(env, v, expected) ? "same" : "other"));
}

static const char *name_of(jmethodID method, char *buf, size_t size)
{
    char *name = NULL;
    if ((*agent_env)->GetMethodName(agent_env, method, &name, NULL, NULL) == JVMTI_ERROR_NONE && name != NULL) {
        snprintf(buf, size, "%s", name);
        (*agent_env)->Deallocate(agent_env, (unsigned char *)name);
    } else {
        snprintf(buf, size, "?");
    }
    return buf;
}

/* The listing's order: `fact stack rows mid main`. */
static void stack_row(void)
{
    jvmtiFrameInfo frames[8];
    jint n = 0;
    jint count = -1;
    char names[256] = "";
    char buf[64];
    jvmtiError err = (*agent_env)->GetFrameCount(agent_env, NULL, &count);
    line("fact frame-count %d", err == JVMTI_ERROR_NONE ? (int)count : -(int)err);
    err = (*agent_env)->GetStackTrace(agent_env, NULL, 0, 8, frames, &n);
    if (err != JVMTI_ERROR_NONE) {
        line("fact stack err=%d", (int)err);
        return;
    }
    for (jint k = 0; k < n; k++) {
        strncat(names, " ", sizeof names - strlen(names) - 1);
        strncat(names, name_of(frames[k].method, buf, sizeof buf), sizeof names - strlen(names) - 1);
    }
    line("fact stack%s", names);
}

/* Is the frame at `depth` listed at an invoke (0xb6-0xba), as HotSpot lists a caller? */
static void at_invoke_row(const char *row, jint depth)
{
    jmethodID method = NULL;
    jlocation location = -1;
    jint size = 0;
    unsigned char *code = NULL;
    jvmtiError err = (*agent_env)->GetFrameLocation(agent_env, NULL, depth, &method, &location);
    if (err != JVMTI_ERROR_NONE) {
        line("fact %s err=%d", row, (int)err);
        return;
    }
    err = (*agent_env)->GetBytecodes(agent_env, method, &size, &code);
    if (err != JVMTI_ERROR_NONE) {
        line("fact %s bytecodes-err=%d", row, (int)err);
        return;
    }
    line("fact %s %s", row,
         (location >= 0 && location < size && code[location] >= 0xb6 && code[location] <= 0xba) ? "true" : "false");
    (*agent_env)->Deallocate(agent_env, code);
}

/*
 * `mid`'s slots (an instance method, -g): this=0, x=1, w=2-3, d=4-5, s=6,
 * probe=7, acc=8, r=9 (not assigned yet at the call). `main`'s: args=0,
 * self=1, sum=2-3, i=4.
 */
JNIEXPORT jstring JNICALL
Java_J27JvmtiCompiledFrameLocals_rows(JNIEnv *env, jclass cls, jobject self, jstring s, jboolean record)
{
    (void)cls;
    if (!record) {
        return NULL;
    }
    out[0] = '\0';
    if (agent_env == NULL) {
        line("fact agent missing");
        return (*env)->NewStringUTF(env, out);
    }
    stack_row();
    at_invoke_row("mid-at-invoke", 1);
    at_invoke_row("main-at-invoke", 2);
    instance_row(env, "mid-this", 1, self);
    int_row("mid-x", 1, 1);
    long_row("mid-w", 1, 2);
    double_row("mid-d", 1, 4);
    object_row(env, "mid-s", 1, 6, s);
    int_row("mid-probe", 1, 7);
    int_row("mid-acc", 1, 8);
    object_row(env, "mid-r", 1, 9, s);
    long_row("mid-x-as-long", 1, 1);
    int_row("mid-slot-past-end", 1, 10);
    object_row(env, "main-self", 2, 1, self);
    long_row("main-sum", 2, 2);
    int_row("main-i", 2, 4);
    return (*env)->NewStringUTF(env, out);
}
