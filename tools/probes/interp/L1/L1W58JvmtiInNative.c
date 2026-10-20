/*
 * Native shim and agent for tools/probes/interp/L1/L1W58JvmtiInNative.java
 * (interpreter round i2 wave 58, lane L1). Loaded twice: as an agent
 * (`-agentpath:...`), whose `Agent_OnLoad` keeps the JVMTI env, and with
 * `System.load`, for the natives. Build instructions are in the probe's
 * header; the Rust port tools/probes/interp/L1/rust-ports/L1W58JvmtiInNative
 * prints the same lines.
 */
#include <jni.h>
#include <jvmti.h>
#include <stdio.h>
#include <string.h>

static jvmtiEnv *agent_env = NULL;
static volatile int spin_running = 0;
static volatile int spin_released = 0;

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

/* "<label>: err=<e> state=0x<the whole word>" for thread `t` (NULL: the
 * current thread). */
static jstring
state_line(JNIEnv *env, jstring label, jthread t)
{
    jvmtiEnv *jvmti = agent_env;
    jint state = 0;
    const char *text;
    char line[256];
    int err;
    if (jvmti == NULL) {
        return (*env)->NewStringUTF(env, "no JVMTI env (load the shim with -agentpath too)\n");
    }
    err = (int)(*jvmti)->GetThreadState(jvmti, t, &state);
    text = (*env)->GetStringUTFChars(env, label, NULL);
    snprintf(line, sizeof line, "%s: err=%d state=0x%x\n", text, err, (unsigned)state);
    (*env)->ReleaseStringUTFChars(env, label, text);
    return (*env)->NewStringUTF(env, line);
}

JNIEXPORT jstring JNICALL
Java_L1W58JvmtiInNative_state(JNIEnv *env, jclass self, jstring label, jthread t)
{
    (void)self;
    return state_line(env, label, t);
}

JNIEXPORT jstring JNICALL
Java_L1W58JvmtiInNative_stateOfSelf(JNIEnv *env, jclass self, jstring label)
{
    (void)self;
    return state_line(env, label, NULL);
}

/* Loops in C, with no JNI call, until `release`. */
JNIEXPORT void JNICALL
Java_L1W58JvmtiInNative_spin(JNIEnv *env, jclass self)
{
    (void)env;
    (void)self;
    spin_running = 1;
    while (!spin_released) {
    }
    spin_running = 0;
    spin_released = 0;
}

JNIEXPORT jboolean JNICALL
Java_L1W58JvmtiInNative_spinning(JNIEnv *env, jclass self)
{
    (void)env;
    (void)self;
    return spin_running ? JNI_TRUE : JNI_FALSE;
}

JNIEXPORT void JNICALL
Java_L1W58JvmtiInNative_release(JNIEnv *env, jclass self)
{
    (void)env;
    (void)self;
    spin_released = 1;
}

/* Calls `r.run()`. */
JNIEXPORT void JNICALL
Java_L1W58JvmtiInNative_upcall(JNIEnv *env, jclass self, jobject r)
{
    jclass klass = (*env)->GetObjectClass(env, r);
    jmethodID run = (*env)->GetMethodID(env, klass, "run", "()V");
    (void)self;
    if (run != NULL) {
        (*env)->CallVoidMethod(env, r, run);
    }
}
