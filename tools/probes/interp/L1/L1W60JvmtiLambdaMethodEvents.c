/*
 * Native shim and agent for tools/probes/interp/L1/L1W60JvmtiLambdaMethodEvents.java
 * (interpreter round i2 wave 60, lane L1). Loaded twice: as an agent
 * (`-agentpath:...`), whose `Agent_OnLoad` adds the method-event
 * capabilities and sets the callbacks, and with `System.load`, for the
 * natives `start` and `stop`. Build instructions are in the probe's header;
 * the Rust port tools/probes/interp/L1/rust-ports/L1W60JvmtiLambdaMethodEvents
 * prints the same lines.
 */
#include <jni.h>
#include <jvmti.h>
#include <stdio.h>
#include <string.h>

static jvmtiEnv *agent_env = NULL;
static int proxy_entries = 0;
static int proxy_exits = 0;
static int body_entries = 0;
static int body_exits = 0;
static char names[512];

/* Counts one event of `method`: a method of a lambda's class or a lambda
 * body. */
static void count(jvmtiEnv *jvmti, jmethodID method, int entry)
{
    jclass declaring = NULL;
    char *csig = NULL;
    char *name = NULL;
    (*jvmti)->GetMethodDeclaringClass(jvmti, method, &declaring);
    (*jvmti)->GetClassSignature(jvmti, declaring, &csig, NULL);
    (*jvmti)->GetMethodName(jvmti, method, &name, NULL, NULL);
    if (csig != NULL && strstr(csig, "$$Lambda") != NULL) {
        if (entry) {
            proxy_entries++;
            if (name != NULL && strlen(names) + strlen(name) + 2 < sizeof names) {
                strcat(names, " ");
                strcat(names, name);
            }
        } else {
            proxy_exits++;
        }
    } else if (name != NULL && strncmp(name, "lambda$main$", 12) == 0) {
        if (entry) {
            body_entries++;
        } else {
            body_exits++;
        }
    }
    (*jvmti)->Deallocate(jvmti, (unsigned char *)csig);
    (*jvmti)->Deallocate(jvmti, (unsigned char *)name);
}

static void JNICALL on_entry(jvmtiEnv *jvmti, JNIEnv *env, jthread thread, jmethodID method)
{
    (void)env;
    (void)thread;
    count(jvmti, method, 1);
}

static void JNICALL on_exit(jvmtiEnv *jvmti, JNIEnv *env, jthread thread, jmethodID method,
                            jboolean popped, jvalue value)
{
    (void)env;
    (void)thread;
    (void)popped;
    (void)value;
    count(jvmti, method, 0);
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
    wanted.can_generate_method_entry_events = 1;
    wanted.can_generate_method_exit_events = 1;
    (*jvmti)->AddCapabilities(jvmti, &wanted);
    memset(&callbacks, 0, sizeof callbacks);
    callbacks.MethodEntry = on_entry;
    callbacks.MethodExit = on_exit;
    (*jvmti)->SetEventCallbacks(jvmti, &callbacks, (jint)sizeof callbacks);
    agent_env = jvmti;
    return 0;
}

JNIEXPORT void JNICALL
Java_L1W60JvmtiLambdaMethodEvents_start(JNIEnv *env, jclass self)
{
    jthread thread = NULL;
    (void)env;
    (void)self;
    if (agent_env == NULL) {
        return;
    }
    proxy_entries = proxy_exits = body_entries = body_exits = 0;
    names[0] = '\0';
    (*agent_env)->GetCurrentThread(agent_env, &thread);
    (*agent_env)->SetEventNotificationMode(agent_env, JVMTI_ENABLE, JVMTI_EVENT_METHOD_ENTRY, thread);
    (*agent_env)->SetEventNotificationMode(agent_env, JVMTI_ENABLE, JVMTI_EVENT_METHOD_EXIT, thread);
}

JNIEXPORT jstring JNICALL
Java_L1W60JvmtiLambdaMethodEvents_stop(JNIEnv *env, jclass self)
{
    jthread thread = NULL;
    char out[1024];
    (void)self;
    if (agent_env == NULL) {
        return (*env)->NewStringUTF(env, "no JVMTI env (load the shim with -agentpath too)\n");
    }
    (*agent_env)->GetCurrentThread(agent_env, &thread);
    (*agent_env)->SetEventNotificationMode(agent_env, JVMTI_DISABLE, JVMTI_EVENT_METHOD_ENTRY, thread);
    (*agent_env)->SetEventNotificationMode(agent_env, JVMTI_DISABLE, JVMTI_EVENT_METHOD_EXIT, thread);
    snprintf(out, sizeof out, "events: proxy entry=%d exit=%d body entry=%d exit=%d names=[%s]\n",
             proxy_entries, proxy_exits, body_entries, body_exits, names);
    return (*env)->NewStringUTF(env, out);
}
