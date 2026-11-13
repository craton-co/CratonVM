/*
 * Native shim and agent for tools/probes/interp/L1/L1W53JvmtiClassHooks.java
 * (interpreter round i2 wave 53, lane L1). Loaded twice: as an agent
 * (`-agentpath:...`), whose `Agent_OnLoad` sets a `ClassFileLoadHook` and a
 * `ClassLoad` callback, and with `System.load`, for the natives `setHooks`
 * and `rows`. Build instructions are in the probe's header; the Rust port
 * tools/probes/interp/L1/rust-ports/L1W53JvmtiClassHooks prints the same lines.
 */
#include <jni.h>
#include <jvmti.h>
#include <stdarg.h>
#include <stdio.h>
#include <string.h>

#define PREFIX "L1W53JvmtiClassHooks$"

static char out[16384];
static jvmtiEnv *agent_env = NULL;
static JavaVM *java_vm = NULL;

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

/* "null" for the bootstrap loader, "app" for the system loader, else "other". */
static const char *loader_kind(JNIEnv *env, jobject loader)
{
    jclass loader_class;
    jmethodID system_loader;
    jobject app;
    if (loader == NULL) {
        return "null";
    }
    loader_class = (*env)->FindClass(env, "java/lang/ClassLoader");
    if (loader_class == NULL) {
        (*env)->ExceptionClear(env);
        return "?";
    }
    system_loader = (*env)->GetStaticMethodID(env, loader_class, "getSystemClassLoader",
                                              "()Ljava/lang/ClassLoader;");
    if (system_loader == NULL) {
        (*env)->ExceptionClear(env);
        return "?";
    }
    app = (*env)->CallStaticObjectMethodA(env, loader_class, system_loader, NULL);
    return (*env)->IsSameObject(env, loader, app) ? "app" : "other";
}

static void JNICALL
on_class_file_load_hook(jvmtiEnv *jvmti, JNIEnv *env, jclass class_being_redefined,
                        jobject loader, const char *name, jobject protection_domain,
                        jint class_data_len, const unsigned char *class_data,
                        jint *new_class_data_len, unsigned char **new_class_data)
{
    jvmtiPhase phase = (jvmtiPhase)0;
    int magic;
    int rewritten = -1;
    if (name == NULL || strncmp(name, PREFIX, strlen(PREFIX)) != 0) {
        return;
    }
    (*jvmti)->GetPhase(jvmti, &phase);
    magic = class_data_len > 4 && class_data[0] == 0xCA && class_data[1] == 0xFE
            && class_data[2] == 0xBA && class_data[3] == 0xBE;
    if (strcmp(name, PREFIX "Patched") == 0) {
        unsigned char *copy = NULL;
        jint i;
        rewritten = 0;
        if ((*jvmti)->Allocate(jvmti, class_data_len, &copy) == JVMTI_ERROR_NONE) {
            memcpy(copy, class_data, (size_t)class_data_len);
            for (i = 0; i + 6 <= class_data_len; i++) {
                if (memcmp(copy + i, "BEFORE", 6) == 0) {
                    memcpy(copy + i, "AFTER!", 6);
                    rewritten++;
                }
            }
            *new_class_data_len = class_data_len;
            *new_class_data = copy;
        }
    }
    if (rewritten < 0) {
        line("hook %s: redefined=%s loader=%s domain=%s magic=%s phase=%d", name,
             class_being_redefined == NULL ? "null" : "class", loader_kind(env, loader),
             protection_domain == NULL ? "null" : "set", magic ? "ok" : "bad", (int)phase);
    } else {
        line("hook %s: redefined=%s loader=%s domain=%s magic=%s phase=%d rewritten=%d", name,
             class_being_redefined == NULL ? "null" : "class", loader_kind(env, loader),
             protection_domain == NULL ? "null" : "set", magic ? "ok" : "bad", (int)phase,
             rewritten);
    }
}

static void JNICALL
on_class_load(jvmtiEnv *jvmti, JNIEnv *env, jthread thread, jclass klass)
{
    char *sig = NULL;
    jint status = -1;
    jthread current = NULL;
    jobject loader = NULL;
    if ((*jvmti)->GetClassSignature(jvmti, klass, &sig, NULL) != JVMTI_ERROR_NONE) {
        return;
    }
    if (strstr(sig, PREFIX) == NULL) {
        (*jvmti)->Deallocate(jvmti, (unsigned char *)sig);
        return;
    }
    (*jvmti)->GetClassStatus(jvmti, klass, &status);
    (*jvmti)->GetCurrentThread(jvmti, &current);
    (*jvmti)->GetClassLoader(jvmti, klass, &loader);
    line("load %s: status=%d thread=%s loader=%s", sig, (int)status,
         (*env)->IsSameObject(env, thread, current) ? "current" : "other",
         loader_kind(env, loader));
    (*jvmti)->Deallocate(jvmti, (unsigned char *)sig);
}

JNIEXPORT jint JNICALL
Agent_OnLoad(JavaVM *vm, char *options, void *reserved)
{
    jvmtiEnv *jvmti = NULL;
    jvmtiEventCallbacks callbacks;
    (void)reserved;
    if (options != NULL && strcmp(options, "fail") == 0) {
        return 7; /* interpreter-L1-a-failed-startup-agent-panics-instead-of-hotspots-init-error-FIXED-20261018 */
    }
    if ((*vm)->GetEnv(vm, (void **)&jvmti, JVMTI_VERSION_1_2) != JNI_OK) {
        return 1;
    }
    memset(&callbacks, 0, sizeof callbacks);
    callbacks.ClassFileLoadHook = &on_class_file_load_hook;
    callbacks.ClassLoad = &on_class_load;
    out[0] = '\0';
    line("onload: callbacks err=%d options=%s",
         (int)(*jvmti)->SetEventCallbacks(jvmti, &callbacks, (jint)sizeof callbacks),
         options == NULL ? "NULL" : options);
    agent_env = jvmti;
    java_vm = vm;
    return 0;
}

JNIEXPORT jstring JNICALL
Java_L1W53JvmtiClassHooks_setHooks(JNIEnv *env, jclass self, jboolean on)
{
    char text[64];
    jvmtiEventMode mode = on ? JVMTI_ENABLE : JVMTI_DISABLE;
    int cflh;
    int load;
    (void)self;
    if (agent_env == NULL) {
        return (*env)->NewStringUTF(env, "no JVMTI env (load the shim with -agentpath too)");
    }
    cflh = (int)(*agent_env)->SetEventNotificationMode(agent_env, mode,
                                                        JVMTI_EVENT_CLASS_FILE_LOAD_HOOK, NULL);
    load = (int)(*agent_env)->SetEventNotificationMode(agent_env, mode, JVMTI_EVENT_CLASS_LOAD,
                                                        NULL);
    snprintf(text, sizeof text, "cflh=%d load=%d", cflh, load);
    return (*env)->NewStringUTF(env, text);
}

/* Which of the four classes in queries GetLoadedClasses lists. */
JNIEXPORT jstring JNICALL
Java_L1W53JvmtiClassHooks_loadedClasses(JNIEnv *env, jclass self, jobjectArray queries)
{
    jint count = 0;
    jclass *classes = NULL;
    int found[4] = { 0, 0, 0, 0 };
    char text[128];
    jint i;
    int q;
    int err;
    (void)self;
    if (agent_env == NULL) {
        return (*env)->NewStringUTF(env, "no JVMTI env (load the shim with -agentpath too)");
    }
    err = (int)(*agent_env)->GetLoadedClasses(agent_env, &count, &classes);
    for (q = 0; q < 4 && err == 0; q++) {
        jobject query = (*env)->GetObjectArrayElement(env, queries, q);
        for (i = 0; i < count; i++) {
            if ((*env)->IsSameObject(env, classes[i], query)) {
                found[q] = 1;
            }
        }
    }
    (*agent_env)->Deallocate(agent_env, (unsigned char *)classes);
    snprintf(text, sizeof text, "err=%d probe=%d target=%d int[]=%d int=%d", err, found[0],
             found[1], found[2], found[3]);
    return (*env)->NewStringUTF(env, text);
}

/* What GetEnv answers for seven JVMTI versions (JNI_OK 0, JNI_EVERSION -3). */
JNIEXPORT jstring JNICALL
Java_L1W53JvmtiClassHooks_envVersions(JNIEnv *env, jclass self)
{
    static const jint versions[7] = {
        0x30090000, 0x30090100, 0x300A0000, 0x300B0100, 0x300C0000, 0x300D0500, 0x301A0000
    };
    char text[160];
    int codes[7];
    int i;
    (void)self;
    for (i = 0; i < 7; i++) {
        void *other = NULL;
        codes[i] = java_vm == NULL ? 1 : (int)(*java_vm)->GetEnv(java_vm, &other, versions[i]);
    }
    snprintf(text, sizeof text, "9.0=%d 9.1=%d 10.0=%d 11.1=%d 12.0=%d 13.5=%d 26.0=%d", codes[0],
             codes[1], codes[2], codes[3], codes[4], codes[5], codes[6]);
    return (*env)->NewStringUTF(env, text);
}

/* Enable, then disable, five events for the current thread only: HotSpot
 * refuses the global-only ones for a thread. */
JNIEXPORT jstring JNICALL
Java_L1W53JvmtiClassHooks_perThread(JNIEnv *env, jclass self)
{
    static const jvmtiEvent events[5] = {
        JVMTI_EVENT_CLASS_FILE_LOAD_HOOK, JVMTI_EVENT_CLASS_LOAD, JVMTI_EVENT_THREAD_START,
        JVMTI_EVENT_THREAD_END, JVMTI_EVENT_CLASS_PREPARE
    };
    int codes[5];
    char text[128];
    jthread thread = NULL;
    int i;
    (void)self;
    if (agent_env == NULL) {
        return (*env)->NewStringUTF(env, "no JVMTI env (load the shim with -agentpath too)");
    }
    (*agent_env)->GetCurrentThread(agent_env, &thread);
    for (i = 0; i < 5; i++) {
        codes[i] = (int)(*agent_env)->SetEventNotificationMode(agent_env, JVMTI_ENABLE,
                                                               events[i], thread);
        (*agent_env)->SetEventNotificationMode(agent_env, JVMTI_DISABLE, events[i], thread);
    }
    snprintf(text, sizeof text, "cflh=%d load=%d start=%d end=%d prepare=%d", codes[0],
             codes[1], codes[2], codes[3], codes[4]);
    return (*env)->NewStringUTF(env, text);
}

JNIEXPORT jstring JNICALL
Java_L1W53JvmtiClassHooks_rows(JNIEnv *env, jclass self)
{
    jstring answer;
    (void)self;
    if (agent_env == NULL) {
        return (*env)->NewStringUTF(env, "no JVMTI env (load the shim with -agentpath too)\n");
    }
    answer = (*env)->NewStringUTF(env, out);
    out[0] = '\0';
    return answer;
}
