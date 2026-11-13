/*
 * Native shim and agent for tools/probes/interp/L1/L1W61JvmtiFrameControl.java
 * (interpreter round i2 wave 61, lane L1). Loaded twice: as an agent
 * (`-agentpath`), whose `Agent_OnLoad` adds `can_suspend`, `can_pop_frame`,
 * `can_force_early_return` and `can_generate_frame_pop_events` and enables
 * `FramePop`, and with `System.load`, for the natives. Build instructions are
 * in the probe's header; the Rust port is
 * tools/probes/interp/L1/rust-ports/L1W61JvmtiFrameControl.
 */
#include <jni.h>
#include <jvmti.h>
#include <stdio.h>
#include <string.h>

static jvmtiEnv *agent_env = NULL;
static jvmtiCapabilities onload_potential;
static int onload_add = -1;
static int onload_enable = -1;
static char pops[2048];

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
    snprintf(item, sizeof item, "  FramePop %s popped=%d\n", name_of(jvmti, method, buf, sizeof buf),
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
    wanted.can_suspend = 1;
    wanted.can_pop_frame = 1;
    wanted.can_force_early_return = 1;
    wanted.can_generate_frame_pop_events = 1;
    onload_add = (int)(*jvmti)->AddCapabilities(jvmti, &wanted);
    memset(&callbacks, 0, sizeof callbacks);
    callbacks.FramePop = on_frame_pop;
    (*jvmti)->SetEventCallbacks(jvmti, &callbacks, (jint)sizeof callbacks);
    onload_enable = (int)(*jvmti)->SetEventNotificationMode(jvmti, JVMTI_ENABLE, JVMTI_EVENT_FRAME_POP, NULL);
    agent_env = jvmti;
    return 0;
}

#define NO_ENV "no JVMTI env (load the shim with -agentpath too)\n"

JNIEXPORT jstring JNICALL
Java_L1W61JvmtiFrameControl_caps(JNIEnv *env, jclass self)
{
    JavaVM *vm = NULL;
    jvmtiEnv *bare = NULL;
    jvmtiCapabilities potential;
    jvmtiCapabilities wanted;
    char out[1024];
    int pop, early, clear, add_pop, add_early;
    (void)self;
    if (agent_env == NULL || (*env)->GetJavaVM(env, &vm) != JNI_OK
        || (*vm)->GetEnv(vm, (void **)&bare, JVMTI_VERSION_1_2) != JNI_OK) {
        return (*env)->NewStringUTF(env, NO_ENV);
    }
    memset(&potential, 0, sizeof potential);
    (*bare)->GetPotentialCapabilities(bare, &potential);
    pop = (int)(*bare)->PopFrame(bare, NULL);
    early = (int)(*bare)->ForceEarlyReturnInt(bare, NULL, 1);
    clear = (int)(*bare)->ClearAllFramePops(bare, NULL);
    memset(&wanted, 0, sizeof wanted);
    wanted.can_pop_frame = 1;
    add_pop = (int)(*bare)->AddCapabilities(bare, &wanted);
    memset(&wanted, 0, sizeof wanted);
    wanted.can_force_early_return = 1;
    add_early = (int)(*bare)->AddCapabilities(bare, &wanted);
    (*bare)->DisposeEnvironment(bare);
    snprintf(out, sizeof out,
             "caps onload: pop=%d early=%d add=%d enable=%d\n"
             "caps live: pop=%d early=%d\n"
             "caps none: pop=%d early=%d clear=%d\n"
             "caps live add: pop=%d early=%d\n",
             (int)onload_potential.can_pop_frame, (int)onload_potential.can_force_early_return, onload_add,
             onload_enable, (int)potential.can_pop_frame, (int)potential.can_force_early_return, pop, early,
             clear, add_pop, add_early);
    return (*env)->NewStringUTF(env, out);
}

JNIEXPORT jint JNICALL
Java_L1W61JvmtiFrameControl_suspend(JNIEnv *env, jclass self, jthread t)
{
    (void)env;
    (void)self;
    return agent_env == NULL ? -1 : (jint)(*agent_env)->SuspendThread(agent_env, t);
}

JNIEXPORT jint JNICALL
Java_L1W61JvmtiFrameControl_resume(JNIEnv *env, jclass self, jthread t)
{
    (void)env;
    (void)self;
    return agent_env == NULL ? -1 : (jint)(*agent_env)->ResumeThread(agent_env, t);
}

JNIEXPORT jint JNICALL
Java_L1W61JvmtiFrameControl_popFrame(JNIEnv *env, jclass self, jthread t)
{
    (void)env;
    (void)self;
    return agent_env == NULL ? -1 : (jint)(*agent_env)->PopFrame(agent_env, t);
}

JNIEXPORT jint JNICALL
Java_L1W61JvmtiFrameControl_frameCount(JNIEnv *env, jclass self, jthread t)
{
    jint count = 0;
    jvmtiError err;
    (void)env;
    (void)self;
    if (agent_env == NULL) {
        return -1;
    }
    err = (*agent_env)->GetFrameCount(agent_env, t, &count);
    return err == JVMTI_ERROR_NONE ? count : -(jint)err;
}

JNIEXPORT jstring JNICALL
Java_L1W61JvmtiFrameControl_frameName(JNIEnv *env, jclass self, jthread t, jint depth)
{
    jmethodID method = NULL;
    jlocation location = 0;
    jvmtiError err;
    char buf[128];
    (void)self;
    if (agent_env == NULL) {
        return (*env)->NewStringUTF(env, NO_ENV);
    }
    err = (*agent_env)->GetFrameLocation(agent_env, t, depth, &method, &location);
    if (err != JVMTI_ERROR_NONE) {
        snprintf(buf, sizeof buf, "err=%d", (int)err);
        return (*env)->NewStringUTF(env, buf);
    }
    return (*env)->NewStringUTF(env, name_of(agent_env, method, buf, sizeof buf));
}

JNIEXPORT jint JNICALL
Java_L1W61JvmtiFrameControl_depthOf(JNIEnv *env, jclass self, jthread t, jstring name)
{
    jint count = 0;
    jint depth;
    const char *want;
    jint found = -1;
    char buf[128];
    (void)self;
    if (agent_env == NULL || (*agent_env)->GetFrameCount(agent_env, t, &count) != JVMTI_ERROR_NONE) {
        return -1;
    }
    want = (*env)->GetStringUTFChars(env, name, NULL);
    for (depth = 0; depth < count && found < 0; depth++) {
        jmethodID method = NULL;
        jlocation location = 0;
        if ((*agent_env)->GetFrameLocation(agent_env, t, depth, &method, &location) == JVMTI_ERROR_NONE
            && strcmp(name_of(agent_env, method, buf, sizeof buf), want) == 0) {
            found = depth;
        }
    }
    (*env)->ReleaseStringUTFChars(env, name, want);
    return found;
}

JNIEXPORT jint JNICALL
Java_L1W61JvmtiFrameControl_earlyInt(JNIEnv *env, jclass self, jthread t, jint v)
{
    (void)env;
    (void)self;
    return agent_env == NULL ? -1 : (jint)(*agent_env)->ForceEarlyReturnInt(agent_env, t, v);
}

JNIEXPORT jint JNICALL
Java_L1W61JvmtiFrameControl_earlyLong(JNIEnv *env, jclass self, jthread t, jlong v)
{
    (void)env;
    (void)self;
    return agent_env == NULL ? -1 : (jint)(*agent_env)->ForceEarlyReturnLong(agent_env, t, v);
}

JNIEXPORT jint JNICALL
Java_L1W61JvmtiFrameControl_earlyFloat(JNIEnv *env, jclass self, jthread t, jfloat v)
{
    (void)env;
    (void)self;
    return agent_env == NULL ? -1 : (jint)(*agent_env)->ForceEarlyReturnFloat(agent_env, t, v);
}

JNIEXPORT jint JNICALL
Java_L1W61JvmtiFrameControl_earlyDouble(JNIEnv *env, jclass self, jthread t, jdouble v)
{
    (void)env;
    (void)self;
    return agent_env == NULL ? -1 : (jint)(*agent_env)->ForceEarlyReturnDouble(agent_env, t, v);
}

JNIEXPORT jint JNICALL
Java_L1W61JvmtiFrameControl_earlyObject(JNIEnv *env, jclass self, jthread t, jobject v)
{
    (void)env;
    (void)self;
    return agent_env == NULL ? -1 : (jint)(*agent_env)->ForceEarlyReturnObject(agent_env, t, v);
}

JNIEXPORT jint JNICALL
Java_L1W61JvmtiFrameControl_earlyVoid(JNIEnv *env, jclass self, jthread t)
{
    (void)env;
    (void)self;
    return agent_env == NULL ? -1 : (jint)(*agent_env)->ForceEarlyReturnVoid(agent_env, t);
}

JNIEXPORT jint JNICALL
Java_L1W61JvmtiFrameControl_notifyFramePop(JNIEnv *env, jclass self, jthread t, jint depth)
{
    (void)env;
    (void)self;
    return agent_env == NULL ? -1 : (jint)(*agent_env)->NotifyFramePop(agent_env, t, depth);
}

JNIEXPORT jint JNICALL
Java_L1W61JvmtiFrameControl_clearAllFramePops(JNIEnv *env, jclass self, jthread t)
{
    (void)env;
    (void)self;
    return agent_env == NULL ? -1 : (jint)(*agent_env)->ClearAllFramePops(agent_env, t);
}

JNIEXPORT jstring JNICALL
Java_L1W61JvmtiFrameControl_events(JNIEnv *env, jclass self)
{
    jstring out;
    (void)self;
    out = (*env)->NewStringUTF(env, pops);
    pops[0] = '\0';
    return out;
}
