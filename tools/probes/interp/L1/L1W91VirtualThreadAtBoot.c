/*
 * Native shim and agent for tools/probes/interp/L1/L1W91VirtualThreadAtBoot.java
 * (interpreter round i4 wave 91, lane L1). Loaded twice: as an agent
 * (`-agentpath:...`), whose `Agent_OnLoad` keeps the JVMTI env and arms
 * `VMInit`, and with `System.load`, for the natives. Build instructions are in
 * the probe's header.
 *
 * `VMInit` enables `ClassPrepare` (live phase on); the callback counts the
 * prepares of `java.lang.VirtualThread` and `java.lang.BaseVirtualThread`.
 * `loaded(sig)` scans `GetLoadedClasses` for a class signature.
 */
#include <jni.h>
#include <jvmti.h>
#include <string.h>

static jvmtiEnv *agent_env = NULL;
static volatile int vt_prepares = 0;
static volatile int base_vt_prepares = 0;

static void JNICALL
on_class_prepare(jvmtiEnv *jvmti, JNIEnv *jni, jthread thread, jclass klass)
{
    char *sig = NULL;
    (void)jni;
    (void)thread;
    if ((*jvmti)->GetClassSignature(jvmti, klass, &sig, NULL) != JVMTI_ERROR_NONE || sig == NULL) {
        return;
    }
    if (strcmp(sig, "Ljava/lang/VirtualThread;") == 0) {
        vt_prepares++;
    } else if (strcmp(sig, "Ljava/lang/BaseVirtualThread;") == 0) {
        base_vt_prepares++;
    }
    (*jvmti)->Deallocate(jvmti, (unsigned char *)sig);
}

static void JNICALL
on_vm_init(jvmtiEnv *jvmti, JNIEnv *jni, jthread thread)
{
    (void)jni;
    (void)thread;
    (*jvmti)->SetEventNotificationMode(jvmti, JVMTI_ENABLE, JVMTI_EVENT_CLASS_PREPARE, NULL);
}

JNIEXPORT jint JNICALL
Agent_OnLoad(JavaVM *vm, char *options, void *reserved)
{
    jvmtiEnv *jvmti = NULL;
    jvmtiEventCallbacks callbacks;
    (void)options;
    (void)reserved;
    if ((*vm)->GetEnv(vm, (void **)&jvmti, JVMTI_VERSION_1_2) != JNI_OK) {
        return 1;
    }
    memset(&callbacks, 0, sizeof callbacks);
    callbacks.VMInit = on_vm_init;
    callbacks.ClassPrepare = on_class_prepare;
    if ((*jvmti)->SetEventCallbacks(jvmti, &callbacks, (jint)sizeof callbacks) != JVMTI_ERROR_NONE) {
        return 1;
    }
    if ((*jvmti)->SetEventNotificationMode(jvmti, JVMTI_ENABLE, JVMTI_EVENT_VM_INIT, NULL)
        != JVMTI_ERROR_NONE) {
        return 1;
    }
    agent_env = jvmti;
    return 0;
}

/* 1 when a loaded class has signature `sig`, 0 when none has, -1 without an
 * env or on a JVMTI error. */
JNIEXPORT jint JNICALL
Java_L1W91VirtualThreadAtBoot_loaded(JNIEnv *env, jclass self, jstring sig)
{
    jvmtiEnv *jvmti = agent_env;
    jint count = 0;
    jclass *classes = NULL;
    const char *want;
    jint found = 0;
    jint i;
    (void)self;
    if (jvmti == NULL) {
        return -1;
    }
    if ((*jvmti)->GetLoadedClasses(jvmti, &count, &classes) != JVMTI_ERROR_NONE) {
        return -1;
    }
    want = (*env)->GetStringUTFChars(env, sig, NULL);
    for (i = 0; i < count; i++) {
        char *s = NULL;
        if (!found && (*jvmti)->GetClassSignature(jvmti, classes[i], &s, NULL) == JVMTI_ERROR_NONE
            && s != NULL) {
            if (strcmp(s, want) == 0) {
                found = 1;
            }
            (*jvmti)->Deallocate(jvmti, (unsigned char *)s);
        }
        (*env)->DeleteLocalRef(env, classes[i]);
    }
    (*env)->ReleaseStringUTFChars(env, sig, want);
    (*jvmti)->Deallocate(jvmti, (unsigned char *)classes);
    return found;
}

/* The ClassPrepare counts since VMInit: VirtualThread * 100 + BaseVirtualThread. */
JNIEXPORT jint JNICALL
Java_L1W91VirtualThreadAtBoot_prepares(JNIEnv *env, jclass self)
{
    (void)env;
    (void)self;
    return (jint)(vt_prepares * 100 + base_vt_prepares);
}
