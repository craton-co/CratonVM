/*
 * Interpreter round i7 wave 139, lane L1: the agent of
 * L1W139JvmtiCallbackParkOnVirtualThread.java. Loaded as an agent
 * (`-agentpath`, for `can_generate_method_entry_events`) and with
 * System.load (for the natives). While armed, its MethodEntry callback calls
 * the Java method `inCallback()V` through JNI each time `marker(I)I` is
 * entered; that method parks, on whatever thread the marker runs on (the
 * probe runs it on virtual threads).
 *
 * Windows (MSVC, from a vcvars64 shell):
 *   cl /nologo /O2 /LD /I"%JAVA_HOME%\include" /I"%JAVA_HOME%\include\win32" L1W139JvmtiCallbackParkOnVirtualThread.c /Fe:%TEMP%\L1W139.dll
 */
#include <jni.h>
#include <jvmti.h>
#include <string.h>
#ifdef _WIN32
#include <windows.h>
#define HIT() InterlockedIncrement(&g_hits)
#else
#define HIT() __sync_fetch_and_add(&g_hits, 1)
#endif

static jvmtiEnv *g_jvmti = NULL;
static jclass g_class = NULL;
static jmethodID g_marker = NULL;
static jmethodID g_in_callback = NULL;
static volatile long g_hits = 0;
static volatile int g_armed = 0;

static void JNICALL
on_method_entry(jvmtiEnv *jvmti, JNIEnv *env, jthread thread, jmethodID method)
{
    (void)jvmti;
    (void)thread;
    if (!g_armed || method != g_marker) {
        return;
    }
    HIT();
    (*env)->CallStaticVoidMethod(env, g_class, g_in_callback);
    if ((*env)->ExceptionCheck(env)) {
        (*env)->ExceptionDescribe(env);
        (*env)->ExceptionClear(env);
    }
}

JNIEXPORT jint JNICALL
Agent_OnLoad(JavaVM *vm, char *options, void *reserved)
{
    jvmtiCapabilities wanted;
    jvmtiEventCallbacks callbacks;
    (void)options;
    (void)reserved;
    if ((*vm)->GetEnv(vm, (void **)&g_jvmti, JVMTI_VERSION_1_2) != JNI_OK) {
        return 1;
    }
    memset(&wanted, 0, sizeof wanted);
    wanted.can_generate_method_entry_events = 1;
    if ((*g_jvmti)->AddCapabilities(g_jvmti, &wanted) != JVMTI_ERROR_NONE) {
        return 2;
    }
    memset(&callbacks, 0, sizeof callbacks);
    callbacks.MethodEntry = on_method_entry;
    if ((*g_jvmti)->SetEventCallbacks(g_jvmti, &callbacks, (jint)sizeof callbacks) != JVMTI_ERROR_NONE) {
        return 3;
    }
    return 0;
}

/* Arm: resolve the two methods, enable MethodEntry for every thread. */
JNIEXPORT jint JNICALL
Java_L1W139JvmtiCallbackParkOnVirtualThread_arm(JNIEnv *env, jclass self)
{
    if (g_jvmti == NULL) {
        return -1;
    }
    g_class = (jclass)(*env)->NewGlobalRef(env, self);
    g_marker = (*env)->GetStaticMethodID(env, self, "marker", "(I)I");
    g_in_callback = (*env)->GetStaticMethodID(env, self, "inCallback", "()V");
    if (g_marker == NULL || g_in_callback == NULL) {
        return -2;
    }
    g_hits = 0;
    g_armed = 1;
    return (jint)(*g_jvmti)->SetEventNotificationMode(g_jvmti, JVMTI_ENABLE, JVMTI_EVENT_METHOD_ENTRY, NULL);
}

/* Disarm: answers the callbacks that matched the marker. */
JNIEXPORT jint JNICALL
Java_L1W139JvmtiCallbackParkOnVirtualThread_disarm(JNIEnv *env, jclass self)
{
    (void)env;
    (void)self;
    g_armed = 0;
    (*g_jvmti)->SetEventNotificationMode(g_jvmti, JVMTI_DISABLE, JVMTI_EVENT_METHOD_ENTRY, NULL);
    return (jint)g_hits;
}
