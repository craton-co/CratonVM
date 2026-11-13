/*
 * Native shim of tools/probes/interp/L1/L1W63JvmtiEarlyHooks.java: the
 * startup agent adds the two early capabilities and
 * `can_generate_all_class_hook_events`, and notes when `VMStart` and the
 * class hook arrive. The Rust port
 * (tools/probes/interp/L1/rust-ports/L1W63JvmtiEarlyHooks) makes the same
 * calls in the same order and prints the same lines.
 */
#include <jvmti.h>
#include <stdio.h>
#include <string.h>

static char onload_out[256];
static jint start_phase = -1;
static int started;
static int hooks_before_start;
static jint first_hook_phase = -1;
static int object_before_start;
static int init_seen;

static jint phase(jvmtiEnv *jvmti) {
    jvmtiPhase p = (jvmtiPhase)-1;
    (*jvmti)->GetPhase(jvmti, &p);
    return (jint)p;
}

static void JNICALL on_vm_start(jvmtiEnv *jvmti, JNIEnv *jni) {
    start_phase = phase(jvmti);
    started = 1;
}

static void JNICALL on_vm_init(jvmtiEnv *jvmti, JNIEnv *jni, jthread t) { init_seen = 1; }

static void JNICALL on_hook(jvmtiEnv *jvmti, JNIEnv *jni, jclass redefined, jobject loader, const char *name,
                            jobject domain, jint len, const unsigned char *data, jint *new_len, unsigned char **new_data) {
    if (first_hook_phase < 0) first_hook_phase = phase(jvmti);
    if (!started) {
        hooks_before_start++;
        if (name != NULL && strcmp(name, "java/lang/Object") == 0) object_before_start = 1;
    }
}

JNIEXPORT jint JNICALL Agent_OnLoad(JavaVM *vm, char *options, void *reserved) {
    jvmtiEnv *jvmti;
    jvmtiCapabilities potential, wanted;
    jvmtiEventCallbacks callbacks;
    jvmtiError e;
    if ((*vm)->GetEnv(vm, (void **)&jvmti, JVMTI_VERSION_1_2) != JNI_OK) return 1;
    memset(&potential, 0, sizeof potential);
    (*jvmti)->GetPotentialCapabilities(jvmti, &potential);
    memset(&wanted, 0, sizeof wanted);
    wanted.can_generate_all_class_hook_events = 1;
    wanted.can_generate_early_vmstart = 1;
    wanted.can_generate_early_class_hook_events = 1;
    e = (*jvmti)->AddCapabilities(jvmti, &wanted);
    snprintf(onload_out, sizeof onload_out, "onload: potential early_vmstart=%d early_class_hook=%d add=%d\n",
             potential.can_generate_early_vmstart, potential.can_generate_early_class_hook_events, e);
    memset(&callbacks, 0, sizeof callbacks);
    callbacks.VMStart = on_vm_start;
    callbacks.VMInit = on_vm_init;
    callbacks.ClassFileLoadHook = on_hook;
    (*jvmti)->SetEventCallbacks(jvmti, &callbacks, sizeof callbacks);
    (*jvmti)->SetEventNotificationMode(jvmti, JVMTI_ENABLE, JVMTI_EVENT_VM_START, NULL);
    (*jvmti)->SetEventNotificationMode(jvmti, JVMTI_ENABLE, JVMTI_EVENT_VM_INIT, NULL);
    (*jvmti)->SetEventNotificationMode(jvmti, JVMTI_ENABLE, JVMTI_EVENT_CLASS_FILE_LOAD_HOOK, NULL);
    return 0;
}

JNIEXPORT jstring JNICALL Java_L1W63JvmtiEarlyHooks_rows(JNIEnv *env, jclass cls) {
    char out[1024];
    snprintf(out, sizeof out,
             "%svmstart: seen=%d phase=%d\nhook: first phase=%d before vmstart=%d object before vmstart=%d\nvminit: seen=%d\n",
             onload_out, started, start_phase, first_hook_phase, hooks_before_start > 0, object_before_start, init_seen);
    return (*env)->NewStringUTF(env, out);
}
