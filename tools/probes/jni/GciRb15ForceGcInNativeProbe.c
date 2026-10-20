/*
 * gci wave i15, lane rb: the agent and natives of
 * tools/probes/GciRb15ForceGcInNativeProbe.java (JVMTI ForceGarbageCollection
 * from a JNI native's C code and from a RunAgentThread start function, both in
 * native). Loaded twice: as -agentpath:<lib> (Agent_OnLoad takes the jvmtiEnv;
 * ForceGarbageCollection needs no capability) and with System.load(<lib>) for
 * the natives.
 *
 *   Windows: cl /LD /O1 /I"%JDK%\include" /I"%JDK%\include\win32" GciRb15ForceGcInNativeProbe.c /Fegcirb15fgc.dll
 *   Linux:   gcc -shared -fPIC -O1 -I$JDK/include -I$JDK/include/linux -o libgcirb15fgc.so GciRb15ForceGcInNativeProbe.c
 */
#include <jni.h>
#include <jvmti.h>

static jvmtiEnv *jvmti;

/* The agent thread's counters (agentStat): done, count, first error. */
static volatile jlong agent_stat[3];
static jint agent_n;

JNIEXPORT jint JNICALL Agent_OnLoad(JavaVM *vm, char *options, void *reserved) {
    (void)options;
    (void)reserved;
    if ((*vm)->GetEnv(vm, (void **)&jvmti, JVMTI_VERSION_1_2) != JNI_OK) {
        return JNI_ERR;
    }
    return JNI_OK;
}

JNIEXPORT jint JNICALL Java_GciRb15ForceGcInNativeProbe_init(JNIEnv *env, jclass cls) {
    (void)env;
    (void)cls;
    return jvmti == NULL ? -1 : 0;
}

JNIEXPORT jint JNICALL Java_GciRb15ForceGcInNativeProbe_forceGc(JNIEnv *env, jclass cls, jint n) {
    jint i;
    (void)env;
    (void)cls;
    if (jvmti == NULL) {
        return -1000;
    }
    for (i = 0; i < n; i++) {
        jvmtiError e = (*jvmti)->ForceGarbageCollection(jvmti);
        if (e != JVMTI_ERROR_NONE) {
            return -(jint)e;
        }
    }
    return 0;
}

static void JNICALL agent_proc(jvmtiEnv *jv, JNIEnv *env, void *arg) {
    jint i;
    (void)env;
    (void)arg;
    for (i = 1; i <= agent_n; i++) {
        jvmtiError e = (*jv)->ForceGarbageCollection(jv);
        if (e != JVMTI_ERROR_NONE && agent_stat[2] == 0) {
            agent_stat[2] = (jlong)e;
        }
        agent_stat[1] = i;
    }
    agent_stat[0] = 1;
}

JNIEXPORT jint JNICALL Java_GciRb15ForceGcInNativeProbe_startAgent(JNIEnv *env, jclass cls, jthread t, jint n) {
    (void)env;
    (void)cls;
    if (jvmti == NULL) {
        return -1000;
    }
    agent_n = n;
    return (jint)(*jvmti)->RunAgentThread(jvmti, t, agent_proc, NULL, JVMTI_THREAD_NORM_PRIORITY);
}

JNIEXPORT jlong JNICALL Java_GciRb15ForceGcInNativeProbe_agentStat(JNIEnv *env, jclass cls, jint which) {
    (void)env;
    (void)cls;
    return which >= 0 && which < 3 ? agent_stat[which] : -1;
}
