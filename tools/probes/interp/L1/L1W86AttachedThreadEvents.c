/*
 * Native shim and agent for tools/probes/interp/L1/L1W86AttachedThreadEvents.java
 * (interpreter round i4 wave 86, lane L1). Loaded twice: as an agent
 * (-agentpath), whose Agent_OnLoad sets ThreadStart / ThreadEnd callbacks and
 * enables both, and with System.load for the native `run`.
 *
 * `run` starts an OS thread that attaches through JNI AttachCurrentThread
 * under the name "l1w86-attached", detaches, and ends; the callbacks count
 * the events whose thread is named so (GetThreadInfo), and `run` answers the
 * two counts. HotSpot posts ThreadStart in attach_current_thread and
 * ThreadEnd in the detach's JavaThread::exit.
 */
#include <jni.h>
#include <jvmti.h>
#include <stdio.h>
#include <string.h>
#ifdef _WIN32
#include <windows.h>
#include <process.h>
#else
#include <pthread.h>
#endif

static jvmtiEnv *agent_env = NULL;
static JavaVM *the_vm = NULL;
static volatile int starts = 0;
static volatile int ends = 0;

static int named_attached(jvmtiEnv *jvmti, jthread thread)
{
    jvmtiThreadInfo info;
    int match;
    memset(&info, 0, sizeof info);
    if ((*jvmti)->GetThreadInfo(jvmti, thread, &info) != JVMTI_ERROR_NONE || info.name == NULL) {
        return 0;
    }
    match = strcmp(info.name, "l1w86-attached") == 0;
    (*jvmti)->Deallocate(jvmti, (unsigned char *)info.name);
    return match;
}

static void JNICALL on_start(jvmtiEnv *jvmti, JNIEnv *env, jthread thread)
{
    (void)env;
    if (named_attached(jvmti, thread)) {
        starts++;
    }
}

static void JNICALL on_end(jvmtiEnv *jvmti, JNIEnv *env, jthread thread)
{
    (void)env;
    if (named_attached(jvmti, thread)) {
        ends++;
    }
}

JNIEXPORT jint JNICALL Agent_OnLoad(JavaVM *vm, char *options, void *reserved)
{
    jvmtiEventCallbacks callbacks;
    (void)options;
    (void)reserved;
    the_vm = vm;
    if ((*vm)->GetEnv(vm, (void **)&agent_env, JVMTI_VERSION_1_2) != JNI_OK) {
        return 1;
    }
    memset(&callbacks, 0, sizeof callbacks);
    callbacks.ThreadStart = on_start;
    callbacks.ThreadEnd = on_end;
    if ((*agent_env)->SetEventCallbacks(agent_env, &callbacks, (jint)sizeof callbacks) != JVMTI_ERROR_NONE) {
        return 2;
    }
    (*agent_env)->SetEventNotificationMode(agent_env, JVMTI_ENABLE, JVMTI_EVENT_THREAD_START, NULL);
    (*agent_env)->SetEventNotificationMode(agent_env, JVMTI_ENABLE, JVMTI_EVENT_THREAD_END, NULL);
    return 0;
}

static void attach_and_detach(void)
{
    JNIEnv *env = NULL;
    JavaVMAttachArgs args;
    args.version = JNI_VERSION_1_8;
    args.name = (char *)"l1w86-attached";
    args.group = NULL;
    if ((*the_vm)->AttachCurrentThread(the_vm, (void **)&env, &args) == JNI_OK) {
        (*the_vm)->DetachCurrentThread(the_vm);
    }
}

#ifdef _WIN32
static unsigned __stdcall body(void *arg)
{
    (void)arg;
    attach_and_detach();
    return 0;
}
#else
static void *body(void *arg)
{
    (void)arg;
    attach_and_detach();
    return NULL;
}
#endif

JNIEXPORT jstring JNICALL Java_L1W86AttachedThreadEvents_run(JNIEnv *env, jclass cls)
{
    char line[128];
    (void)cls;
    if (agent_env == NULL || the_vm == NULL) {
        return (*env)->NewStringUTF(env, "no agent");
    }
#ifdef _WIN32
    {
        HANDLE t = (HANDLE)_beginthreadex(NULL, 0, body, NULL, 0, NULL);
        WaitForSingleObject(t, INFINITE);
        CloseHandle(t);
    }
#else
    {
        pthread_t t;
        pthread_create(&t, NULL, body, NULL);
        pthread_join(t, NULL);
    }
#endif
    snprintf(line, sizeof line, "attached thread: ThreadStart=%d ThreadEnd=%d", starts, ends);
    return (*env)->NewStringUTF(env, line);
}
