/*
 * Agent for tools/probes/interp/L1/L1W94JvmtiDeadPhase.java (interpreter
 * round i4 wave 94, lane L1; stage 2 of
 * docs/known-issues/interpreter/i54-L1-proposal-one-jvmti-phase-per-vm-20261018.md).
 * `VMInit` keeps global references to `java.lang.Object`, its `hashCode`
 * method and the `VMInit` thread; `VMDeath` (still the live phase) and
 * `Agent_OnUnload` (the dead phase) call the same functions with them and
 * print each answer (`fflush`ed). Build instructions are in the probe's
 * header.
 */
#include <jni.h>
#include <jvmti.h>
#include <stdio.h>
#include <string.h>

static jvmtiEnv *agent_env = NULL;
static jclass object_class = NULL;
static jmethodID hash_code = NULL;
static jobject a_thread = NULL;

static int phase_of(jvmtiEnv *jvmti)
{
    jvmtiPhase phase = (jvmtiPhase)0;
    (*jvmti)->GetPhase(jvmti, &phase);
    return (int)phase;
}

/* The calls, in one row: the start-or-live ones (GetClassSignature,
 * GetClassModifiers, GetMethodName, GetMaxLocals, GetObjectHashCode,
 * GetCurrentThread), a live-only one (GetAllThreads) and three any-phase
 * ones (GetTime, GetErrorName, Allocate). */
static void row(const char *label, jvmtiEnv *jvmti)
{
    char *sig = NULL;
    char *name = NULL;
    char *msig = NULL;
    char *err_name = NULL;
    unsigned char *mem = NULL;
    jint modifiers = 0;
    jint max_locals = 0;
    jint hash = 0;
    jthread current = NULL;
    jint count = 0;
    jthread *threads = NULL;
    jlong nanos = 0;
    int c_sig, c_mod, c_name, c_locals, c_hash, c_current, c_threads, c_time, c_err, c_alloc;
    c_sig = (int)(*jvmti)->GetClassSignature(jvmti, object_class, &sig, NULL);
    c_mod = (int)(*jvmti)->GetClassModifiers(jvmti, object_class, &modifiers);
    c_name = (int)(*jvmti)->GetMethodName(jvmti, hash_code, &name, &msig, NULL);
    c_locals = (int)(*jvmti)->GetMaxLocals(jvmti, hash_code, &max_locals);
    c_hash = (int)(*jvmti)->GetObjectHashCode(jvmti, a_thread, &hash);
    c_current = (int)(*jvmti)->GetCurrentThread(jvmti, &current);
    c_threads = (int)(*jvmti)->GetAllThreads(jvmti, &count, &threads);
    c_time = (int)(*jvmti)->GetTime(jvmti, &nanos);
    c_err = (int)(*jvmti)->GetErrorName(jvmti, JVMTI_ERROR_WRONG_PHASE, &err_name);
    c_alloc = (int)(*jvmti)->Allocate(jvmti, 16, &mem);
    printf("%s: phase=%d signature=%d modifiers=%d method=%d locals=%d hash=%d current=%d "
           "threads=%d time=%d errname=%d allocate=%d\n",
           label, phase_of(jvmti), c_sig, c_mod, c_name, c_locals, c_hash, c_current,
           c_threads, c_time, c_err, c_alloc);
    fflush(stdout);
    if (c_sig == JVMTI_ERROR_NONE) {
        (*jvmti)->Deallocate(jvmti, (unsigned char *)sig);
    }
    if (c_name == JVMTI_ERROR_NONE) {
        (*jvmti)->Deallocate(jvmti, (unsigned char *)name);
        (*jvmti)->Deallocate(jvmti, (unsigned char *)msig);
    }
    if (c_threads == JVMTI_ERROR_NONE) {
        (*jvmti)->Deallocate(jvmti, (unsigned char *)threads);
    }
    if (c_err == JVMTI_ERROR_NONE) {
        (*jvmti)->Deallocate(jvmti, (unsigned char *)err_name);
    }
    if (c_alloc == JVMTI_ERROR_NONE) {
        (*jvmti)->Deallocate(jvmti, mem);
    }
}

static void JNICALL
on_vm_init(jvmtiEnv *jvmti, JNIEnv *env, jthread thread)
{
    jclass object = (*env)->FindClass(env, "java/lang/Object");
    (void)jvmti;
    if (object == NULL) {
        (*env)->ExceptionClear(env);
        return;
    }
    object_class = (jclass)(*env)->NewGlobalRef(env, object);
    hash_code = (*env)->GetMethodID(env, object, "hashCode", "()I");
    a_thread = (*env)->NewGlobalRef(env, thread);
}

static void JNICALL
on_vm_death(jvmtiEnv *jvmti, JNIEnv *env)
{
    (void)env;
    if (object_class != NULL) {
        row("vmdeath", jvmti);
    }
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
    callbacks.VMInit = &on_vm_init;
    callbacks.VMDeath = &on_vm_death;
    (*jvmti)->SetEventCallbacks(jvmti, &callbacks, (jint)sizeof callbacks);
    (*jvmti)->SetEventNotificationMode(jvmti, JVMTI_ENABLE, JVMTI_EVENT_VM_INIT, NULL);
    (*jvmti)->SetEventNotificationMode(jvmti, JVMTI_ENABLE, JVMTI_EVENT_VM_DEATH, NULL);
    agent_env = jvmti;
    return 0;
}

JNIEXPORT void JNICALL
Agent_OnUnload(JavaVM *vm)
{
    (void)vm;
    if (agent_env != NULL && object_class != NULL) {
        row("unload", agent_env);
    }
}
