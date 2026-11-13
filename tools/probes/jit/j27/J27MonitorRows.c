/*
 * Native shim and agent for tools/probes/jit/j27/J27MonitorRows.java (j27
 * wave 4, lane MONROWS, SDB17-5). Loaded twice: as an agent
 * (`-agentpath:...`), whose `Agent_OnLoad` acquires
 * can_generate_exception_events and sets `Exception` and `ExceptionCatch`
 * callbacks (both events stay disabled), and with `System.load`, for the
 * natives that arm, disarm and count. Only throwables of the probe's own
 * class (`J27MonitorRows$Tc`, handed to `arm`) are counted, and no callback
 * calls Java. Build instructions are in the probe's header.
 */
#include <jni.h>
#include <jvmti.h>
#include <string.h>

static jvmtiEnv *agent_env = NULL;
static jclass marker = NULL;
static volatile int exception_events = 0;
static volatile int catch_events = 0;
static volatile int catch_in_tcb = 0;
static volatile int catch_in_tcm = 0;

static int is_marker(JNIEnv *env, jobject exception)
{
    return exception != NULL && marker != NULL && (*env)->IsInstanceOf(env, exception, marker);
}

static void JNICALL
on_exception(jvmtiEnv *jvmti, JNIEnv *env, jthread thread, jmethodID method,
             jlocation location, jobject exception, jmethodID catch_method,
             jlocation catch_location)
{
    (void)jvmti;
    (void)thread;
    (void)method;
    (void)location;
    (void)catch_method;
    (void)catch_location;
    if (is_marker(env, exception)) {
        exception_events++;
    }
}

static void JNICALL
on_exception_catch(jvmtiEnv *jvmti, JNIEnv *env, jthread thread, jmethodID method,
                   jlocation location, jobject exception)
{
    char *name = NULL;
    (void)thread;
    (void)location;
    if (!is_marker(env, exception)) {
        return;
    }
    catch_events++;
    if ((*jvmti)->GetMethodName(jvmti, method, &name, NULL, NULL) == JVMTI_ERROR_NONE) {
        if (strcmp(name, "tcb") == 0) {
            catch_in_tcb++;
        } else if (strcmp(name, "tcm") == 0) {
            catch_in_tcm++;
        }
        (*jvmti)->Deallocate(jvmti, (unsigned char *)name);
    }
}

JNIEXPORT jint JNICALL
Agent_OnLoad(JavaVM *vm, char *options, void *reserved)
{
    jvmtiEnv *jvmti = NULL;
    jvmtiCapabilities wanted;
    jvmtiEventCallbacks callbacks;
    (void)options;
    (void)reserved;
    if ((*vm)->GetEnv(vm, (void **)&jvmti, JVMTI_VERSION_1_2) != JNI_OK) {
        return 1;
    }
    memset(&wanted, 0, sizeof wanted);
    wanted.can_generate_exception_events = 1;
    (*jvmti)->AddCapabilities(jvmti, &wanted);
    memset(&callbacks, 0, sizeof callbacks);
    callbacks.Exception = &on_exception;
    callbacks.ExceptionCatch = &on_exception_catch;
    (*jvmti)->SetEventCallbacks(jvmti, &callbacks, (jint)sizeof callbacks);
    agent_env = jvmti;
    return 0;
}

static void set_mode(jvmtiEventMode mode)
{
    jthread thread = NULL;
    if (agent_env == NULL) {
        return;
    }
    (*agent_env)->GetCurrentThread(agent_env, &thread);
    (*agent_env)->SetEventNotificationMode(agent_env, mode, JVMTI_EVENT_EXCEPTION, thread);
    (*agent_env)->SetEventNotificationMode(agent_env, mode, JVMTI_EVENT_EXCEPTION_CATCH, thread);
}

JNIEXPORT void JNICALL
Java_J27MonitorRows_arm(JNIEnv *env, jclass self, jclass marker_class)
{
    (void)self;
    marker = (jclass)(*env)->NewGlobalRef(env, marker_class);
    set_mode(JVMTI_ENABLE);
}

JNIEXPORT void JNICALL
Java_J27MonitorRows_disarm(JNIEnv *env, jclass self)
{
    (void)env;
    (void)self;
    set_mode(JVMTI_DISABLE);
}

JNIEXPORT jint JNICALL
Java_J27MonitorRows_exceptionEvents(JNIEnv *env, jclass self)
{
    (void)env;
    (void)self;
    return exception_events;
}

JNIEXPORT jint JNICALL
Java_J27MonitorRows_catchEvents(JNIEnv *env, jclass self)
{
    (void)env;
    (void)self;
    return catch_events;
}

JNIEXPORT jint JNICALL
Java_J27MonitorRows_catchesIn(JNIEnv *env, jclass self, jstring method)
{
    const char *text;
    jint answer = -1;
    (void)self;
    text = (*env)->GetStringUTFChars(env, method, NULL);
    if (text == NULL) {
        return answer;
    }
    if (strcmp(text, "tcb") == 0) {
        answer = catch_in_tcb;
    } else if (strcmp(text, "tcm") == 0) {
        answer = catch_in_tcm;
    }
    (*env)->ReleaseStringUTFChars(env, method, text);
    return answer;
}
