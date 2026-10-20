/*
 * Native shim and agent for tools/probes/interp/L1/L1W134JvmtiBlockedEntryFrameControl.java
 * (interpreter round i6 wave 134, lane L1; i133-L1 and i133-L8). Loaded twice:
 * as an agent (`-agentpath`), whose `Agent_OnLoad` adds `can_suspend`,
 * `can_pop_frame`, `can_force_early_return` and
 * `can_generate_method_entry_events` and sets the `MethodEntry` callback, and
 * with `System.load`, for the natives. Build instructions are in the probe's
 * header.
 *
 * Actions (space-separated tokens; each calls one function on the named
 * thread and records `token=<jvmtiError>`): `int:N` ForceEarlyReturnInt(N),
 * `long` ForceEarlyReturnLong(77), `void` ForceEarlyReturnVoid, `pop`
 * PopFrame.
 */
#include <jni.h>
#include <jvmti.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static jvmtiEnv *agent_env = NULL;
static int entry_armed = 0;
static char entry_target[64];
static char entry_actions[256];
static int entry_hits = 0;
static int entry_acted = 0;
static char entry_record[512];

static void
append(char *record, size_t size, const char *item)
{
    if (strlen(record) + strlen(item) + 1 < size) {
        strcat(record, item);
    }
}

static void
run_actions(jvmtiEnv *jvmti, jthread thread, const char *acts, char *record, size_t size)
{
    char copy[256];
    char item[64];
    char *token;
    snprintf(copy, sizeof copy, "%s", acts);
    for (token = strtok(copy, " "); token != NULL; token = strtok(NULL, " ")) {
        jvmtiError err;
        if (strncmp(token, "int:", 4) == 0) {
            err = (*jvmti)->ForceEarlyReturnInt(jvmti, thread, atoi(token + 4));
        } else if (strcmp(token, "long") == 0) {
            err = (*jvmti)->ForceEarlyReturnLong(jvmti, thread, 77);
        } else if (strcmp(token, "void") == 0) {
            err = (*jvmti)->ForceEarlyReturnVoid(jvmti, thread);
        } else if (strcmp(token, "pop") == 0) {
            err = (*jvmti)->PopFrame(jvmti, thread);
        } else {
            err = (jvmtiError)-1;
        }
        snprintf(item, sizeof item, "%s=%d ", token, (int)err);
        append(record, size, item);
    }
}

static void JNICALL
on_method_entry(jvmtiEnv *jvmti, JNIEnv *env, jthread thread, jmethodID method)
{
    char *name = NULL;
    int matched;
    (void)env;
    if (!entry_armed) {
        return;
    }
    if ((*jvmti)->GetMethodName(jvmti, method, &name, NULL, NULL) != JVMTI_ERROR_NONE || name == NULL) {
        return;
    }
    matched = strcmp(name, entry_target) == 0;
    (*jvmti)->Deallocate(jvmti, (unsigned char *)name);
    if (!matched) {
        return;
    }
    entry_hits++;
    if (entry_acted) {
        return;
    }
    entry_acted = 1;
    run_actions(jvmti, thread, entry_actions, entry_record, sizeof entry_record);
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
    memset(&wanted, 0, sizeof wanted);
    wanted.can_suspend = 1;
    wanted.can_pop_frame = 1;
    wanted.can_force_early_return = 1;
    wanted.can_generate_method_entry_events = 1;
    if ((*jvmti)->AddCapabilities(jvmti, &wanted) != JVMTI_ERROR_NONE) {
        return 1;
    }
    memset(&callbacks, 0, sizeof callbacks);
    callbacks.MethodEntry = on_method_entry;
    (*jvmti)->SetEventCallbacks(jvmti, &callbacks, (jint)sizeof callbacks);
    agent_env = jvmti;
    return 0;
}

/* Suspend `thread`, run `acts` on it, resume it: `suspend=<e> <token=e>... resume=<e>`. */
JNIEXPORT jstring JNICALL
Java_L1W134JvmtiBlockedEntryFrameControl_suspended(JNIEnv *env, jclass self, jthread thread, jstring acts)
{
    char record[512];
    char item[64];
    const char *a;
    jvmtiError err;
    (void)self;
    if (agent_env == NULL) {
        return (*env)->NewStringUTF(env, "no JVMTI env (load the shim with -agentpath too)");
    }
    record[0] = '\0';
    err = (*agent_env)->SuspendThread(agent_env, thread);
    snprintf(item, sizeof item, "suspend=%d ", (int)err);
    append(record, sizeof record, item);
    a = (*env)->GetStringUTFChars(env, acts, NULL);
    run_actions(agent_env, thread, a, record, sizeof record);
    (*env)->ReleaseStringUTFChars(env, acts, a);
    err = (*agent_env)->ResumeThread(agent_env, thread);
    snprintf(item, sizeof item, "resume=%d", (int)err);
    append(record, sizeof record, item);
    return (*env)->NewStringUTF(env, record);
}

/* Enable `MethodEntry` for `thread`; the callback acts once, for `method`. */
JNIEXPORT jint JNICALL
Java_L1W134JvmtiBlockedEntryFrameControl_armEntry(JNIEnv *env, jclass self, jthread thread, jstring method,
                                                  jstring acts)
{
    const char *m;
    const char *a;
    (void)self;
    if (agent_env == NULL) {
        return -1;
    }
    m = (*env)->GetStringUTFChars(env, method, NULL);
    a = (*env)->GetStringUTFChars(env, acts, NULL);
    snprintf(entry_target, sizeof entry_target, "%s", m);
    snprintf(entry_actions, sizeof entry_actions, "%s", a);
    (*env)->ReleaseStringUTFChars(env, method, m);
    (*env)->ReleaseStringUTFChars(env, acts, a);
    entry_hits = 0;
    entry_acted = 0;
    entry_record[0] = '\0';
    entry_armed = 1;
    return (jint)(*agent_env)->SetEventNotificationMode(agent_env, JVMTI_ENABLE, JVMTI_EVENT_METHOD_ENTRY, thread);
}

/* Disable `MethodEntry` for `thread`: `<token=e>... hits=<n>`. */
JNIEXPORT jstring JNICALL
Java_L1W134JvmtiBlockedEntryFrameControl_disarmEntry(JNIEnv *env, jclass self, jthread thread)
{
    char out[640];
    (void)self;
    if (agent_env == NULL) {
        return (*env)->NewStringUTF(env, "no JVMTI env (load the shim with -agentpath too)");
    }
    (*agent_env)->SetEventNotificationMode(agent_env, JVMTI_DISABLE, JVMTI_EVENT_METHOD_ENTRY, thread);
    entry_armed = 0;
    snprintf(out, sizeof out, "%shits=%d", entry_record, entry_hits);
    return (*env)->NewStringUTF(env, out);
}

/* `CallStaticIntMethod(klass, name, "()I")`: the callee's caller is this native. */
JNIEXPORT jint JNICALL
Java_L1W134JvmtiBlockedEntryFrameControl_callStaticInt(JNIEnv *env, jclass self, jstring name)
{
    const char *n = (*env)->GetStringUTFChars(env, name, NULL);
    jmethodID m = (*env)->GetStaticMethodID(env, self, n, "()I");
    jint r = -1;
    (*env)->ReleaseStringUTFChars(env, name, n);
    if (m != NULL) {
        r = (*env)->CallStaticIntMethod(env, self, m);
    }
    return r;
}
