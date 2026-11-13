/*
 * Agent for tools/probes/interp/L1/L1W95JvmtiCheckOrder.java (interpreter
 * round i4 wave 95, lane L1). `VMStart` (start phase) sets and reads the
 * calling thread's local storage; `VMInit` (live phase) calls
 * `GetClassMethods` and `GetMethodDeclaringClass` with NULL arguments in
 * several orders and keeps global references to `java.lang.Object` and its
 * `hashCode`; `Agent_OnUnload` (dead phase) calls the four functions again.
 * Every row is `fflush`ed. Build instructions are in the probe's header.
 */
#include <jni.h>
#include <jvmti.h>
#include <stdio.h>
#include <string.h>

static jvmtiEnv *agent_env = NULL;
static jclass object_class = NULL;
static jmethodID hash_code = NULL;
static int storage_cookie = 0;

static int phase_of(jvmtiEnv *jvmti)
{
    jvmtiPhase phase = (jvmtiPhase)0;
    (*jvmti)->GetPhase(jvmti, &phase);
    return (int)phase;
}

static void JNICALL
on_vm_start(jvmtiEnv *jvmti, JNIEnv *env)
{
    void *mine = NULL;
    int set, get, null_out;
    (void)env;
    set = (int)(*jvmti)->SetThreadLocalStorage(jvmti, NULL, &storage_cookie);
    get = (int)(*jvmti)->GetThreadLocalStorage(jvmti, NULL, &mine);
    null_out = (int)(*jvmti)->GetThreadLocalStorage(jvmti, NULL, NULL);
    printf("start: phase=%d set=%d get=%d same=%d get-null-out=%d\n", phase_of(jvmti), set, get,
           mine == (void *)&storage_cookie ? 1 : 0, null_out);
    fflush(stdout);
}

static void JNICALL
on_vm_init(jvmtiEnv *jvmti, JNIEnv *env, jthread thread)
{
    jclass object = (*env)->FindClass(env, "java/lang/Object");
    jint count = 0;
    jmethodID *methods = NULL;
    void *mine = NULL;
    int null_class, null_out, null_method, declaring_null_out, storage;
    (void)thread;
    if (object == NULL) {
        (*env)->ExceptionClear(env);
        return;
    }
    object_class = (jclass)(*env)->NewGlobalRef(env, object);
    hash_code = (*env)->GetMethodID(env, object, "hashCode", "()I");
    null_class = (int)(*jvmti)->GetClassMethods(jvmti, NULL, NULL, NULL);
    null_out = (int)(*jvmti)->GetClassMethods(jvmti, object_class, &count, NULL);
    null_method = (int)(*jvmti)->GetMethodDeclaringClass(jvmti, NULL, NULL);
    declaring_null_out = (int)(*jvmti)->GetMethodDeclaringClass(jvmti, hash_code, NULL);
    storage = (int)(*jvmti)->GetThreadLocalStorage(jvmti, NULL, &mine);
    printf("live: methods-null-class=%d methods-null-out=%d declaring-null-method=%d "
           "declaring-null-out=%d storage=%d same=%d\n",
           null_class, null_out, null_method, declaring_null_out, storage,
           mine == (void *)&storage_cookie ? 1 : 0);
    fflush(stdout);
    if (methods != NULL) {
        (*jvmti)->Deallocate(jvmti, (unsigned char *)methods);
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
    callbacks.VMStart = &on_vm_start;
    callbacks.VMInit = &on_vm_init;
    (*jvmti)->SetEventCallbacks(jvmti, &callbacks, (jint)sizeof callbacks);
    (*jvmti)->SetEventNotificationMode(jvmti, JVMTI_ENABLE, JVMTI_EVENT_VM_START, NULL);
    (*jvmti)->SetEventNotificationMode(jvmti, JVMTI_ENABLE, JVMTI_EVENT_VM_INIT, NULL);
    agent_env = jvmti;
    return 0;
}

JNIEXPORT void JNICALL
Agent_OnUnload(JavaVM *vm)
{
    void *mine = NULL;
    int methods, declaring, set, get;
    (void)vm;
    if (agent_env == NULL || object_class == NULL) {
        return;
    }
    methods = (int)agent_env[0]->GetClassMethods(agent_env, object_class, NULL, NULL);
    declaring = (int)agent_env[0]->GetMethodDeclaringClass(agent_env, hash_code, NULL);
    set = (int)agent_env[0]->SetThreadLocalStorage(agent_env, NULL, NULL);
    get = (int)agent_env[0]->GetThreadLocalStorage(agent_env, NULL, &mine);
    printf("unload: phase=%d methods=%d declaring=%d set=%d get=%d\n", phase_of(agent_env),
           methods, declaring, set, get);
    fflush(stdout);
}
