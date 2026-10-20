/*
 * Native shim and agent for tools/probes/interp/L7/L7W62ObserverArmedMidLoop.java
 * (interpreter round i2 wave 62, lane L7). Loaded twice: as an agent
 * (`-agentpath`), whose `Agent_OnLoad` adds the single-step, method-entry,
 * breakpoint and line-number capabilities and sets the three callbacks, and
 * with `System.load`, for the natives. Build instructions are in the probe's
 * header; the Rust port is
 * tools/probes/interp/L7/rust-ports/L7W62ObserverArmedMidLoop.
 */
#include <jni.h>
#include <jvmti.h>
#include <string.h>

static jvmtiEnv *agent_env = NULL;

/* The probe's `count` indices: SPIN_STEP, TICK, SPIN_BREAK, SPIN_SELF. */
static const char *const known[] = {"spinStep", "tick", "spinBreak", "spinSelf"};
#define KNOWN 4
static volatile int counts[KNOWN];

static void
note(jvmtiEnv *jvmti, jmethodID method)
{
    char *name = NULL;
    int i;
    if ((*jvmti)->GetMethodName(jvmti, method, &name, NULL, NULL) != JVMTI_ERROR_NONE || name == NULL) {
        return;
    }
    for (i = 0; i < KNOWN; i++) {
        if (strcmp(known[i], name) == 0) {
            counts[i]++;
            break;
        }
    }
    (*jvmti)->Deallocate(jvmti, (unsigned char *)name);
}

static void JNICALL
on_single_step(jvmtiEnv *jvmti, JNIEnv *env, jthread thread, jmethodID method, jlocation location)
{
    (void)env;
    (void)thread;
    (void)location;
    note(jvmti, method);
}

static void JNICALL
on_breakpoint(jvmtiEnv *jvmti, JNIEnv *env, jthread thread, jmethodID method, jlocation location)
{
    (void)env;
    (void)thread;
    (void)location;
    note(jvmti, method);
}

static void JNICALL
on_method_entry(jvmtiEnv *jvmti, JNIEnv *env, jthread thread, jmethodID method)
{
    (void)env;
    (void)thread;
    note(jvmti, method);
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
    wanted.can_generate_single_step_events = 1;
    wanted.can_generate_method_entry_events = 1;
    wanted.can_generate_breakpoint_events = 1;
    wanted.can_get_line_numbers = 1;
    (*jvmti)->AddCapabilities(jvmti, &wanted);
    memset(&callbacks, 0, sizeof callbacks);
    callbacks.SingleStep = on_single_step;
    callbacks.Breakpoint = on_breakpoint;
    callbacks.MethodEntry = on_method_entry;
    (*jvmti)->SetEventCallbacks(jvmti, &callbacks, (jint)sizeof callbacks);
    /* Breakpoint events for every thread once; nothing posts one until a
       breakpoint is set. */
    (*jvmti)->SetEventNotificationMode(jvmti, JVMTI_ENABLE, JVMTI_EVENT_BREAKPOINT, NULL);
    agent_env = jvmti;
    return 0;
}

JNIEXPORT jint JNICALL
Java_L7W62ObserverArmedMidLoop_setEvent(JNIEnv *env, jclass self, jboolean on, jint event, jthread thread)
{
    (void)env;
    (void)self;
    if (agent_env == NULL) {
        return -1;
    }
    return (jint)(*agent_env)->SetEventNotificationMode(agent_env, on ? JVMTI_ENABLE : JVMTI_DISABLE,
                                                        (jvmtiEvent)event, thread);
}

JNIEXPORT jint JNICALL
Java_L7W62ObserverArmedMidLoop_breakpoints(JNIEnv *env, jclass self, jclass owner, jboolean on)
{
    jmethodID method;
    jint entries = 0;
    jvmtiLineNumberEntry *table = NULL;
    jvmtiError err;
    jint i;
    jint first = 0;
    (void)self;
    if (agent_env == NULL) {
        return -1;
    }
    method = (*env)->GetStaticMethodID(env, owner, "spinBreak", "()V");
    if (method == NULL) {
        (*env)->ExceptionClear(env);
        return -2;
    }
    err = (*agent_env)->GetLineNumberTable(agent_env, method, &entries, &table);
    if (err != JVMTI_ERROR_NONE) {
        return (jint)err;
    }
    for (i = 0; i < entries; i++) {
        err = on ? (*agent_env)->SetBreakpoint(agent_env, method, table[i].start_location)
                 : (*agent_env)->ClearBreakpoint(agent_env, method, table[i].start_location);
        if (err != JVMTI_ERROR_NONE && first == 0) {
            first = (jint)err;
        }
    }
    (*agent_env)->Deallocate(agent_env, (unsigned char *)table);
    return first;
}

JNIEXPORT jint JNICALL
Java_L7W62ObserverArmedMidLoop_count(JNIEnv *env, jclass self, jint which)
{
    (void)env;
    (void)self;
    if (which < 0 || which >= KNOWN) {
        return -1;
    }
    return counts[which];
}

JNIEXPORT void JNICALL
Java_L7W62ObserverArmedMidLoop_reset(JNIEnv *env, jclass self)
{
    int i;
    (void)env;
    (void)self;
    for (i = 0; i < KNOWN; i++) {
        counts[i] = 0;
    }
}
