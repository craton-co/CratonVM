/*
 * Native shim and agent for tools/probes/interp/L1/L1W91JvmtiStateSamplerBench.java
 * (interpreter round i4 wave 91, lane L1). Loaded twice: as an agent
 * (`-agentpath:...`), whose `Agent_OnLoad` keeps the JVMTI env, and with
 * `System.load`, for the native `sample`. Build instructions are in the
 * probe's header.
 *
 * `sample(threads, rounds)`: `rounds` passes of `GetThreadState` over every
 * thread of the array, as a profiler's sampler does; returns the OR of every
 * state word read (a checksum), or -1 without an env, -2 on a JVMTI error.
 */
#include <jni.h>
#include <jvmti.h>

#define MAX_THREADS 64

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

JNIEXPORT jint JNICALL
Java_L1W91JvmtiStateSamplerBench_sample(JNIEnv *env, jclass self, jobjectArray threads, jint rounds)
{
    jvmtiEnv *jvmti = agent_env;
    jthread ts[MAX_THREADS];
    jint n;
    jint i;
    jint r;
    jint acc = 0;
    (void)self;
    if (jvmti == NULL) {
        return -1;
    }
    n = (*env)->GetArrayLength(env, threads);
    if (n > MAX_THREADS) {
        n = MAX_THREADS;
    }
    for (i = 0; i < n; i++) {
        ts[i] = (jthread)(*env)->GetObjectArrayElement(env, threads, i);
    }
    for (r = 0; r < rounds; r++) {
        for (i = 0; i < n; i++) {
            jint state = 0;
            if ((*jvmti)->GetThreadState(jvmti, ts[i], &state) != JVMTI_ERROR_NONE) {
                acc = -2;
                goto done;
            }
            acc |= state;
        }
    }
done:
    for (i = 0; i < n; i++) {
        (*env)->DeleteLocalRef(env, ts[i]);
    }
    return acc;
}
