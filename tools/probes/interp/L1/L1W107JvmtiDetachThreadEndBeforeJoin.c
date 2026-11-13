/*
 * Native shim and agent for tools/probes/interp/L1/L1W107JvmtiDetachThreadEndBeforeJoin.java
 * (interpreter round i5 wave 107, lane L1). Loaded twice: as an agent
 * (-agentpath), whose Agent_OnLoad enables ThreadStart / ThreadEnd, and with
 * System.load for the natives `say`, `startAttached` and `awaitAttached`
 * (the last two also for the JDI scenario L1W107JdiAttachedThreadHeldAtThreadDeath,
 * which loads it with System.load only).
 *
 * `startAttached` starts an OS thread that attaches through JNI
 * AttachCurrentThread under the name "l1w107-attached", calls the probe's
 * static `fromNative()` once, and detaches. The agent's ThreadEnd for that
 * thread prints whether Thread.isAlive() still answers true, then sleeps
 * 300 ms before it returns, so a joiner that wakes before the end of the
 * callback prints its line in between. All output goes through this file's
 * stdio (`say`), flushed per line, so the lines of the two threads keep
 * their real order.
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
#include <time.h>
#endif

static jvmtiEnv *agent_env = NULL;
static JavaVM *the_vm = NULL;
static jclass probe_class = NULL;
static jclass thread_class = NULL;
static jmethodID from_native = NULL;
static jmethodID is_alive = NULL;
#ifdef _WIN32
static HANDLE attached;
#else
static pthread_t attached;
#endif

static void say_line(const char *line)
{
    printf("%s\n", line);
    fflush(stdout);
}

static void pause_ms(int ms)
{
#ifdef _WIN32
    Sleep((DWORD)ms);
#else
    struct timespec ts;
    ts.tv_sec = ms / 1000;
    ts.tv_nsec = (long)(ms % 1000) * 1000000L;
    nanosleep(&ts, NULL);
#endif
}

static int named_attached(jvmtiEnv *jvmti, jthread thread)
{
    jvmtiThreadInfo info;
    int match;
    memset(&info, 0, sizeof info);
    if ((*jvmti)->GetThreadInfo(jvmti, thread, &info) != JVMTI_ERROR_NONE || info.name == NULL) {
        return 0;
    }
    match = strcmp(info.name, "l1w107-attached") == 0;
    (*jvmti)->Deallocate(jvmti, (unsigned char *)info.name);
    return match;
}

static void JNICALL on_start(jvmtiEnv *jvmti, JNIEnv *env, jthread thread)
{
    (void)env;
    if (named_attached(jvmti, thread)) {
        say_line("ThreadStart l1w107-attached");
    }
}

static void JNICALL on_end(jvmtiEnv *jvmti, JNIEnv *env, jthread thread)
{
    char line[128];
    jboolean alive = JNI_FALSE;
    if (!named_attached(jvmti, thread)) {
        return;
    }
    if (is_alive != NULL) {
        alive = (*env)->CallBooleanMethod(env, thread, is_alive);
        if ((*env)->ExceptionCheck(env)) {
            (*env)->ExceptionClear(env);
            say_line("ThreadEnd l1w107-attached isAlive threw");
        }
    }
    snprintf(line, sizeof line, "ThreadEnd l1w107-attached alive=%s", alive ? "true" : "false");
    say_line(line);
    pause_ms(300);
    say_line("ThreadEnd l1w107-attached returns");
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

static void attach_call_detach(void)
{
    JNIEnv *env = NULL;
    JavaVMAttachArgs args;
    args.version = JNI_VERSION_1_8;
    args.name = (char *)"l1w107-attached";
    args.group = NULL;
    if ((*the_vm)->AttachCurrentThread(the_vm, (void **)&env, &args) != JNI_OK) {
        say_line("attach failed");
        return;
    }
    (*env)->CallStaticVoidMethod(env, probe_class, from_native);
    if ((*env)->ExceptionCheck(env)) {
        (*env)->ExceptionClear(env);
        say_line("fromNative threw");
    }
    if ((*the_vm)->DetachCurrentThread(the_vm) != JNI_OK) {
        say_line("detach failed");
    }
}

#ifdef _WIN32
static unsigned __stdcall body(void *arg)
{
    (void)arg;
    attach_call_detach();
    return 0;
}
#else
static void *body(void *arg)
{
    (void)arg;
    attach_call_detach();
    return NULL;
}
#endif

JNIEXPORT void JNICALL Java_L1W107JvmtiDetachThreadEndBeforeJoin_say(JNIEnv *env, jclass cls, jstring s)
{
    const char *chars;
    (void)cls;
    chars = (*env)->GetStringUTFChars(env, s, NULL);
    if (chars != NULL) {
        say_line(chars);
        (*env)->ReleaseStringUTFChars(env, s, chars);
    }
}

/* `cls` is the class whose static `fromNative()V` the attached thread calls. */
static jboolean start_attached(JNIEnv *env, jclass cls)
{
    jclass tc;
    if (the_vm == NULL && (*env)->GetJavaVM(env, &the_vm) != JNI_OK) {
        return JNI_FALSE;
    }
    probe_class = (jclass)(*env)->NewGlobalRef(env, cls);
    from_native = (*env)->GetStaticMethodID(env, cls, "fromNative", "()V");
    tc = (*env)->FindClass(env, "java/lang/Thread");
    if (from_native == NULL || tc == NULL) {
        return JNI_FALSE;
    }
    thread_class = (jclass)(*env)->NewGlobalRef(env, tc);
    is_alive = (*env)->GetMethodID(env, thread_class, "isAlive", "()Z");
    if (is_alive == NULL) {
        return JNI_FALSE;
    }
#ifdef _WIN32
    attached = (HANDLE)_beginthreadex(NULL, 0, body, NULL, 0, NULL);
#else
    if (pthread_create(&attached, NULL, body, NULL) != 0) {
        return JNI_FALSE;
    }
#endif
    return JNI_TRUE;
}

static void await_attached(void)
{
#ifdef _WIN32
    WaitForSingleObject(attached, INFINITE);
    CloseHandle(attached);
#else
    pthread_join(attached, NULL);
#endif
}

JNIEXPORT jboolean JNICALL Java_L1W107JvmtiDetachThreadEndBeforeJoin_startAttached(JNIEnv *env, jclass cls)
{
    if (agent_env == NULL) {
        return JNI_FALSE;
    }
    return start_attached(env, cls);
}

JNIEXPORT void JNICALL Java_L1W107JvmtiDetachThreadEndBeforeJoin_awaitAttached(JNIEnv *env, jclass cls)
{
    (void)env;
    (void)cls;
    await_attached();
}

/*
 * The same natives for the JDI scenario
 * tools/probes/interp/L1/L1W107JdiAttachedThreadHeldAtThreadDeath.java, which
 * loads this library with System.load only (no agent: the JDWP back end is
 * the observer there).
 */
JNIEXPORT jboolean JNICALL Java_L1W107JdiAttachedThreadHeldAtThreadDeath_startAttached(JNIEnv *env, jclass cls)
{
    return start_attached(env, cls);
}

JNIEXPORT void JNICALL Java_L1W107JdiAttachedThreadHeldAtThreadDeath_awaitAttached(JNIEnv *env, jclass cls)
{
    (void)env;
    (void)cls;
    await_attached();
}
