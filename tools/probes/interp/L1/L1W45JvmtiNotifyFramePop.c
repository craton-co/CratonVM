/*
 * Native shim and agent for tools/probes/interp/L1/L1W45JvmtiNotifyFramePop.java
 * (interpreter round i1 wave 45, lane L1). Loaded twice: as an agent
 * (`-agentpath`), whose `Agent_OnLoad` adds `can_generate_frame_pop_events`
 * and sets the `FramePop` callback, and with `System.load`, for the natives
 * `rows(other, which)` (NotifyFramePop calls, one line each) and `events()`
 * (the FramePop callbacks recorded so far). The rows that need a capability
 * to fail use a second env, obtained in the live phase, which has none.
 * Build instructions are in the probe's header.
 */
#include <jni.h>
#include <jvmti.h>
#include <stdarg.h>
#include <stdio.h>
#include <string.h>

static char out[4096];
static char pops[1024];
static jvmtiEnv *agent_env = NULL;
static jvmtiCapabilities onload_potential;
static int onload_add = -1;
static int onload_callbacks = -1;

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

static void JNICALL
on_frame_pop(jvmtiEnv *jvmti, JNIEnv *env, jthread thread, jmethodID method,
             jboolean was_popped_by_exception)
{
    char buf[128];
    char item[200];
    (void)env;
    (void)thread;
    snprintf(item, sizeof item, "FramePop %s popped=%d\n", name_of(jvmti, method, buf, sizeof buf),
             (int)was_popped_by_exception);
    if (strlen(pops) + strlen(item) + 1 < sizeof pops) {
        strcat(pops, item);
    }
}

JNIEXPORT jint JNICALL
Agent_OnLoad(JavaVM *vm, char *options, void *reserved)
{
    jvmtiEnv *jvmti = NULL;
    jvmtiCapabilities wanted;
    jvmtiEventCallbacks callbacks;
    (void)options;
    (void)reserved;
    if ((*vm)->GetEnv(vm, (void **)&jvmti, JVMTI_VERSION_1_2) != JNI_OK) {
        return 1;
    }
    memset(&onload_potential, 0, sizeof onload_potential);
    (*jvmti)->GetPotentialCapabilities(jvmti, &onload_potential);
    memset(&wanted, 0, sizeof wanted);
    wanted.can_generate_frame_pop_events = 1;
    onload_add = (int)(*jvmti)->AddCapabilities(jvmti, &wanted);
    memset(&callbacks, 0, sizeof callbacks);
    callbacks.FramePop = on_frame_pop;
    onload_callbacks = (int)(*jvmti)->SetEventCallbacks(jvmti, &callbacks, (jint)sizeof callbacks);
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

#define ERR_ROW(row, call) line("%s: err=%d", row, (int)(call))

JNIEXPORT jstring JNICALL
Java_L1W45JvmtiNotifyFramePop_rows(JNIEnv *env, jclass self, jthread other, jint which)
{
    JavaVM *vm = NULL;
    jvmtiEnv *jvmti = agent_env;
    jvmtiEnv *bare = NULL;
    jvmtiCapabilities potential;
    jvmtiCapabilities wanted;
    jvmtiError err;
    (void)self;
    out[0] = '\0';
    if (jvmti == NULL || (*env)->GetJavaVM(env, &vm) != JNI_OK
        || (*vm)->GetEnv(vm, (void **)&bare, JVMTI_VERSION_1_2) != JNI_OK) {
        return (*env)->NewStringUTF(env, "no JVMTI env (load the shim with -agentpath too)\n");
    }
    if (which == 2) {
        ERR_ROW("NotifyFramePop thrower", (*jvmti)->NotifyFramePop(jvmti, NULL, 1));
        return (*env)->NewStringUTF(env, out);
    }
    memset(&potential, 0, sizeof potential);
    err = (*bare)->GetPotentialCapabilities(bare, &potential);
    line("live-phase potential: err=%d can_generate_frame_pop_events=%d", (int)err,
         (int)potential.can_generate_frame_pop_events);
    line("Agent_OnLoad potential: can_generate_frame_pop_events=%d",
         (int)onload_potential.can_generate_frame_pop_events);
    line("Agent_OnLoad AddCapabilities: err=%d SetEventCallbacks: err=%d", onload_add,
         onload_callbacks);
    ERR_ROW("no capability NotifyFramePop", (*bare)->NotifyFramePop(bare, NULL, 1));
    ERR_ROW("no capability enable FramePop",
            (*bare)->SetEventNotificationMode(bare, JVMTI_ENABLE, JVMTI_EVENT_FRAME_POP, NULL));
    memset(&wanted, 0, sizeof wanted);
    wanted.can_generate_frame_pop_events = 1;
    ERR_ROW("AddCapabilities in the live phase", (*bare)->AddCapabilities(bare, &wanted));
    ERR_ROW("enable FramePop",
            (*jvmti)->SetEventNotificationMode(jvmti, JVMTI_ENABLE, JVMTI_EVENT_FRAME_POP, NULL));
    ERR_ROW("NotifyFramePop depth 0 (the native)", (*jvmti)->NotifyFramePop(jvmti, NULL, 0));
    ERR_ROW("NotifyFramePop depth 1 (caller)", (*jvmti)->NotifyFramePop(jvmti, NULL, 1));
    ERR_ROW("NotifyFramePop depth 1 again", (*jvmti)->NotifyFramePop(jvmti, NULL, 1));
    ERR_ROW("NotifyFramePop depth -1", (*jvmti)->NotifyFramePop(jvmti, NULL, -1));
    ERR_ROW("NotifyFramePop depth 99", (*jvmti)->NotifyFramePop(jvmti, NULL, 99));
    ERR_ROW("NotifyFramePop of a running other thread", (*jvmti)->NotifyFramePop(jvmti, other, 1));
    ERR_ROW("NotifyFramePop of a class", (*jvmti)->NotifyFramePop(jvmti, (jthread)self, 1));
    return (*env)->NewStringUTF(env, out);
}

JNIEXPORT jstring JNICALL
Java_L1W45JvmtiNotifyFramePop_events(JNIEnv *env, jclass self)
{
    (void)self;
    return (*env)->NewStringUTF(env, pops);
}
