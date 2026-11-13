/*
 * Native shim and agent for tools/probes/interp/L1/L1W52JvmtiExceptionCatch.java
 * (interpreter round i1 wave 52, lane L1). Loaded twice: as an agent
 * (`-agentpath:...`), whose `Agent_OnLoad` acquires
 * can_generate_exception_events and sets an `ExceptionCatch` callback, and
 * with `System.load`, for the native `rows`. Build instructions are in the
 * probe's header.
 */
#include <jni.h>
#include <jvmti.h>
#include <stdarg.h>
#include <stdio.h>
#include <string.h>

static char out[8192];
static jvmtiEnv *agent_env = NULL;
static int onload_caps = -1;
static int onload_callbacks = -1;
static jclass probe_class = NULL;
static int hot = 0;

static void line(const char *fmt, ...)
{
    char item[512];
    va_list ap;
    va_start(ap, fmt);
    vsnprintf(item, sizeof item, fmt, ap);
    va_end(ap);
    if (strlen(out) + strlen(item) + 2 < sizeof out) {
        strcat(out, item);
        strcat(out, "\n");
    }
}

static void JNICALL
on_exception_catch(jvmtiEnv *jvmti, JNIEnv *env, jthread thread, jmethodID method,
                   jlocation location, jobject exception)
{
    jclass declaring = NULL;
    char *name = NULL;
    char *sig = NULL;
    jclass exception_class;
    jmethodID get_message;
    jstring message;
    const char *text = "null";
    (void)thread;
    if (probe_class == NULL) {
        return;
    }
    if ((*jvmti)->GetMethodDeclaringClass(jvmti, method, &declaring) != JVMTI_ERROR_NONE
        || !(*env)->IsSameObject(env, declaring, probe_class)) {
        return;
    }
    if (exception == NULL) {
        line("catch: no exception object");
        return;
    }
    exception_class = (*env)->GetObjectClass(env, exception);
    get_message = (*env)->GetMethodID(env, exception_class, "getMessage", "()Ljava/lang/String;");
    message = (jstring)(*env)->CallObjectMethod(env, exception, get_message);
    if (message != NULL) {
        text = (*env)->GetStringUTFChars(env, message, NULL);
    }
    if (strcmp(text, "hot") == 0) {
        hot++;
    } else {
        (*jvmti)->GetMethodName(jvmti, method, &name, NULL, NULL);
        (*jvmti)->GetClassSignature(jvmti, exception_class, &sig, NULL);
        line("catch in %s at %d: %s \"%s\"", name, (int)location, sig, text);
        (*jvmti)->Deallocate(jvmti, (unsigned char *)name);
        (*jvmti)->Deallocate(jvmti, (unsigned char *)sig);
    }
    if (message != NULL) {
        (*env)->ReleaseStringUTFChars(env, message, text);
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
    onload_caps = (int)(*jvmti)->AddCapabilities(jvmti, &wanted);
    memset(&callbacks, 0, sizeof callbacks);
    callbacks.ExceptionCatch = &on_exception_catch;
    onload_callbacks = (int)(*jvmti)->SetEventCallbacks(jvmti, &callbacks, (jint)sizeof callbacks);
    agent_env = jvmti;
    return 0;
}

JNIEXPORT jstring JNICALL
Java_L1W52JvmtiExceptionCatch_rows(JNIEnv *env, jclass self)
{
    jthread thread = NULL;
    jmethodID exercise;
    int enable;
    int disable;
    out[0] = '\0';
    if (agent_env == NULL) {
        return (*env)->NewStringUTF(env, "no JVMTI env (load the shim with -agentpath too)\n");
    }
    line("onload: caps err=%d callbacks err=%d", onload_caps, onload_callbacks);
    probe_class = (jclass)(*env)->NewGlobalRef(env, self);
    (*agent_env)->GetCurrentThread(agent_env, &thread);
    enable = (int)(*agent_env)->SetEventNotificationMode(agent_env, JVMTI_ENABLE,
                                                          JVMTI_EVENT_EXCEPTION_CATCH, thread);
    line("enable: err=%d", enable);
    exercise = (*env)->GetStaticMethodID(env, self, "exercise", "()V");
    (*env)->CallStaticVoidMethod(env, self, exercise);
    disable = (int)(*agent_env)->SetEventNotificationMode(agent_env, JVMTI_DISABLE,
                                                           JVMTI_EVENT_EXCEPTION_CATCH, thread);
    line("disable: err=%d", disable);
    line("hot catches: %d", hot);
    return (*env)->NewStringUTF(env, out);
}
