/*
 * Native shim and agent for tools/probes/interp/L1/L1W52JvmtiThreadAndClassFacts.java
 * (interpreter round i1 wave 52, lane L1). Loaded twice: as an agent
 * (`-agentpath:...`), whose `Agent_OnLoad` acquires can_get_source_file_name
 * and can_get_synthetic_attribute, creates a raw monitor and stores a pointer
 * in its env's local storage, and with `System.load`, for the native `rows`.
 * Build instructions are in the probe's header.
 */
#include <jni.h>
#include <jvmti.h>
#include <stdarg.h>
#include <stdio.h>
#include <string.h>

static char out[16384];
static jvmtiEnv *agent_env = NULL;
static jrawMonitorID monitor = NULL;
static int storage_cookie = 52;
static int onload_caps = -1;
static int onload_monitor = -1;
static int onload_storage = -1;
static int onload_events = -1;
static char thread_events[512];

/* ThreadStart / ThreadEnd of the probe's own threads (named "facts-"). */
static void record_thread_event(jvmtiEnv *jvmti, jthread thread, const char *what)
{
    jvmtiThreadInfo info;
    memset(&info, 0, sizeof info);
    if ((*jvmti)->GetThreadInfo(jvmti, thread, &info) != JVMTI_ERROR_NONE) {
        return;
    }
    if (strncmp(info.name, "facts-", 6) == 0
        && strlen(thread_events) + strlen(info.name) + 16 < sizeof thread_events) {
        strcat(thread_events, " ");
        strcat(thread_events, what);
        strcat(thread_events, " ");
        strcat(thread_events, info.name);
    }
    (*jvmti)->Deallocate(jvmti, (unsigned char *)info.name);
}

static void JNICALL on_thread_start(jvmtiEnv *jvmti, JNIEnv *env, jthread thread)
{
    (void)env;
    record_thread_event(jvmti, thread, "start");
}

static void JNICALL on_thread_end(jvmtiEnv *jvmti, JNIEnv *env, jthread thread)
{
    (void)env;
    record_thread_event(jvmti, thread, "end");
}

JNIEXPORT jint JNICALL
Agent_OnLoad(JavaVM *vm, char *options, void *reserved)
{
    jvmtiEnv *jvmti = NULL;
    jvmtiCapabilities wanted;
    jvmtiEventCallbacks callbacks;
    int e1;
    int e2;
    int e3;
    (void)options;
    (void)reserved;
    if ((*vm)->GetEnv(vm, (void **)&jvmti, JVMTI_VERSION_1_2) != JNI_OK) {
        return 1;
    }
    memset(&wanted, 0, sizeof wanted);
    wanted.can_get_source_file_name = 1;
    wanted.can_get_synthetic_attribute = 1;
    wanted.can_get_bytecodes = 1;
    onload_caps = (int)(*jvmti)->AddCapabilities(jvmti, &wanted);
    onload_monitor = (int)(*jvmti)->CreateRawMonitor(jvmti, "l1w52-facts", &monitor);
    onload_storage = (int)(*jvmti)->SetEnvironmentLocalStorage(jvmti, &storage_cookie);
    memset(&callbacks, 0, sizeof callbacks);
    callbacks.ThreadStart = &on_thread_start;
    callbacks.ThreadEnd = &on_thread_end;
    e1 = (int)(*jvmti)->SetEventCallbacks(jvmti, &callbacks, (jint)sizeof callbacks);
    e2 = (int)(*jvmti)->SetEventNotificationMode(jvmti, JVMTI_ENABLE, JVMTI_EVENT_THREAD_START, NULL);
    e3 = (int)(*jvmti)->SetEventNotificationMode(jvmti, JVMTI_ENABLE, JVMTI_EVENT_THREAD_END, NULL);
    onload_events = e1 != 0 ? e1 : e2 != 0 ? e2 : e3;
    agent_env = jvmti;
    return 0;
}

static void line(const char *fmt, ...)
{
    char item[1024];
    va_list ap;
    va_start(ap, fmt);
    vsnprintf(item, sizeof item, fmt, ap);
    va_end(ap);
    if (strlen(out) + strlen(item) + 2 < sizeof out) {
        strcat(out, item);
        strcat(out, "\n");
    }
}

static void dealloc(void *p)
{
    if (p != NULL) {
        (*agent_env)->Deallocate(agent_env, (unsigned char *)p);
    }
}

/* A class's signature, into `buf`. */
static void signature_of(jclass klass, char *buf, size_t size)
{
    char *sig = NULL;
    if (klass == NULL) {
        snprintf(buf, size, "null");
        return;
    }
    if ((*agent_env)->GetClassSignature(agent_env, klass, &sig, NULL) != JVMTI_ERROR_NONE) {
        snprintf(buf, size, "?");
        return;
    }
    snprintf(buf, size, "%s", sig);
    dealloc(sig);
}

/* A thread group's name, into `buf` ("null" for NULL). */
static void group_name(jthreadGroup group, char *buf, size_t size)
{
    jvmtiThreadGroupInfo info;
    if (group == NULL) {
        snprintf(buf, size, "null");
        return;
    }
    memset(&info, 0, sizeof info);
    if ((*agent_env)->GetThreadGroupInfo(agent_env, group, &info) != JVMTI_ERROR_NONE) {
        snprintf(buf, size, "?");
        return;
    }
    snprintf(buf, size, "%s", info.name);
    dealloc(info.name);
}

static const char *loader_kind(JNIEnv *env, jobject loader, jobject app)
{
    if (loader == NULL) {
        return "null";
    }
    return (*env)->IsSameObject(env, loader, app) ? "app" : "other";
}

static void thread_row(JNIEnv *env, const char *row, jthread thread, jobject app)
{
    jvmtiThreadInfo info;
    char group[128];
    int err;
    memset(&info, 0, sizeof info);
    err = (int)(*agent_env)->GetThreadInfo(agent_env, thread, &info);
    if (err != JVMTI_ERROR_NONE) {
        line("thread %s: err=%d", row, err);
        return;
    }
    group_name(info.thread_group, group, sizeof group);
    line("thread %s: err=%d name=%s prio=%d daemon=%d group=%s loader=%s", row, err, info.name,
         (int)info.priority, (int)info.is_daemon, group,
         loader_kind(env, info.context_class_loader, app));
    dealloc(info.name);
}

static void group_row(jthreadGroup group)
{
    jvmtiThreadGroupInfo info;
    char parent[128];
    int err;
    memset(&info, 0, sizeof info);
    err = (int)(*agent_env)->GetThreadGroupInfo(agent_env, group, &info);
    if (err != JVMTI_ERROR_NONE) {
        line("group: err=%d", err);
        return;
    }
    group_name(info.parent, parent, sizeof parent);
    line("group %s: err=%d name=%s parent=%s max=%d daemon=%d", info.name, err, info.name, parent,
         (int)info.max_priority, (int)info.is_daemon);
    dealloc(info.name);
}

static void class_row(JNIEnv *env, jclass klass, jobject app)
{
    char sig[256];
    char source[160];
    char version[32];
    char interfaces[1024];
    char one[256];
    jint modifiers = -1;
    jboolean is_interface = 2;
    jboolean is_array = 2;
    char *source_name = NULL;
    jobject loader = NULL;
    jint count = 0;
    jclass *list = NULL;
    jint minor = 0;
    jint major = 0;
    int err;
    int methods_err;
    int i;
    signature_of(klass, sig, sizeof sig);
    (*agent_env)->GetClassModifiers(agent_env, klass, &modifiers);
    (*agent_env)->IsInterface(agent_env, klass, &is_interface);
    (*agent_env)->IsArrayClass(agent_env, klass, &is_array);
    err = (int)(*agent_env)->GetSourceFileName(agent_env, klass, &source_name);
    if (err == JVMTI_ERROR_NONE) {
        snprintf(source, sizeof source, "0:%s", source_name);
        dealloc(source_name);
    } else {
        snprintf(source, sizeof source, "%d", err);
    }
    (*agent_env)->GetClassLoader(agent_env, klass, &loader);
    err = (int)(*agent_env)->GetImplementedInterfaces(agent_env, klass, &count, &list);
    if (err == JVMTI_ERROR_NONE) {
        snprintf(interfaces, sizeof interfaces, "%d", (int)count);
        for (i = 0; i < count; i++) {
            signature_of(list[i], one, sizeof one);
            if (strlen(interfaces) + strlen(one) + 2 < sizeof interfaces) {
                strcat(interfaces, " ");
                strcat(interfaces, one);
            }
        }
        dealloc(list);
    } else {
        snprintf(interfaces, sizeof interfaces, "err=%d", err);
    }
    {
        jint method_count = 0;
        jmethodID *methods = NULL;
        methods_err = (int)(*agent_env)->GetClassMethods(agent_env, klass, &method_count, &methods);
        dealloc(methods);
    }
    err = (int)(*agent_env)->GetClassVersionNumbers(agent_env, klass, &minor, &major);
    if (err == JVMTI_ERROR_NONE) {
        snprintf(version, sizeof version, "0:%d", (int)major);
    } else {
        snprintf(version, sizeof version, "%d", err);
    }
    line("class %s: mods=0x%x interface=%d array=%d source=%s loader=%s interfaces=%s methods=%d version=%s",
         sig, (unsigned)modifiers, (int)is_interface, (int)is_array, source,
         loader_kind(env, loader, app), interfaces, methods_err, version);
}

static void field_row(jclass klass, jfieldID field)
{
    char *name = NULL;
    char *sig = NULL;
    char *generic = NULL;
    jint modifiers = -1;
    jclass declaring = NULL;
    char declaring_sig[256];
    jboolean synthetic = 2;
    int err;
    err = (int)(*agent_env)->GetFieldName(agent_env, klass, field, &name, &sig, &generic);
    if (err != JVMTI_ERROR_NONE) {
        line("field: err=%d", err);
        return;
    }
    (*agent_env)->GetFieldModifiers(agent_env, klass, field, &modifiers);
    (*agent_env)->GetFieldDeclaringClass(agent_env, klass, field, &declaring);
    (*agent_env)->IsFieldSynthetic(agent_env, klass, field, &synthetic);
    signature_of(declaring, declaring_sig, sizeof declaring_sig);
    line("field %s %s generic=%s mods=0x%x declaring=%s synthetic=%d", name, sig,
         generic == NULL ? "-" : generic, (unsigned)modifiers, declaring_sig, (int)synthetic);
    dealloc(name);
    dealloc(sig);
    dealloc(generic);
}

static void fields_rows(const char *row, jclass klass)
{
    jint count = 0;
    jfieldID *fields = NULL;
    int err;
    int i;
    err = (int)(*agent_env)->GetClassFields(agent_env, klass, &count, &fields);
    line("fields of %s: err=%d count=%d", row, err, (int)count);
    for (i = 0; err == JVMTI_ERROR_NONE && i < count; i++) {
        field_row(klass, fields[i]);
    }
    dealloc(fields);
}

/* The name `GetFieldName` gives a JNI field id. */
static void field_name_of(jclass klass, jfieldID field, char *buf, size_t size)
{
    char *name = NULL;
    int err = (int)(*agent_env)->GetFieldName(agent_env, klass, field, &name, NULL, NULL);
    if (err != JVMTI_ERROR_NONE) {
        snprintf(buf, size, "err=%d", err);
        return;
    }
    snprintf(buf, size, "%s", name);
    dealloc(name);
}

static void method_row(JNIEnv *env, jclass klass, const char *name, const char *sig, int is_static)
{
    jmethodID method = is_static ? (*env)->GetStaticMethodID(env, klass, name, sig)
                                 : (*env)->GetMethodID(env, klass, name, sig);
    jint max_locals = -1;
    jint args_size = -1;
    jboolean synthetic = 2;
    int e1;
    int e2;
    int e3;
    if (method == NULL) {
        (*env)->ExceptionClear(env);
        line("method %s: missing", name);
        return;
    }
    {
        jint count = -1;
        unsigned char *codes = NULL;
        int err = (int)(*agent_env)->GetBytecodes(agent_env, method, &count, &codes);
        char hex[64];
        hex[0] = '\0';
        if (err == JVMTI_ERROR_NONE) {
            jint k;
            for (k = 0; k < count && k < 16; k++) {
                char two[4];
                snprintf(two, sizeof two, "%02x", codes[k]);
                strcat(hex, two);
            }
            dealloc(codes);
        }
        line("bytecodes %s: err=%d count=%d %s", name, err, (int)count, hex);
    }
    e1 = (int)(*agent_env)->GetMaxLocals(agent_env, method, &max_locals);
    e2 = (int)(*agent_env)->GetArgumentsSize(agent_env, method, &args_size);
    e3 = (int)(*agent_env)->IsMethodSynthetic(agent_env, method, &synthetic);
    line("method %s: maxLocals=%d:%d argsSize=%d:%d synthetic=%d:%d", name, e1, (int)max_locals, e2,
         (int)args_size, e3, (int)synthetic);
}

JNIEXPORT jstring JNICALL
Java_L1W52JvmtiThreadAndClassFacts_rows(JNIEnv *env, jclass self, jthread worker,
                                        jthread unstarted, jobject app, jobjectArray classes,
                                        jclass pair, jclass inner)
{
    jint phase = -1;
    void *storage = NULL;
    int err;
    jvmtiThreadInfo info;
    jint count = 0;
    jthreadGroup *groups = NULL;
    char name[128];
    char parent[128];
    jsize i;
    jfieldID created;
    jfieldID count_field;
    char created_name[64];
    char count_name[64];
    (void)self;
    out[0] = '\0';
    if (agent_env == NULL) {
        return (*env)->NewStringUTF(env, "no JVMTI env (load the shim with -agentpath too)\n");
    }
    line("onload: caps err=%d monitor err=%d storage err=%d events err=%d", onload_caps,
         onload_monitor, onload_storage, onload_events);
    thread_row(env, "NULL", NULL, app);

    err = (int)(*agent_env)->GetPhase(agent_env, &phase);
    line("phase: err=%d phase=%d", err, (int)phase);
    err = (int)(*agent_env)->GetEnvironmentLocalStorage(agent_env, &storage);
    line("storage: err=%d same=%d", err, storage == (void *)&storage_cookie);

    {
        int a = (int)(*agent_env)->RawMonitorEnter(agent_env, monitor);
        int b = (int)(*agent_env)->RawMonitorEnter(agent_env, monitor);
        int c = (int)(*agent_env)->RawMonitorExit(agent_env, monitor);
        int d = (int)(*agent_env)->RawMonitorWait(agent_env, monitor, 1);
        int e = (int)(*agent_env)->RawMonitorNotify(agent_env, monitor);
        int f = (int)(*agent_env)->RawMonitorNotifyAll(agent_env, monitor);
        int g = (int)(*agent_env)->RawMonitorExit(agent_env, monitor);
        int h = (int)(*agent_env)->RawMonitorExit(agent_env, monitor);
        int k = (int)(*agent_env)->DestroyRawMonitor(agent_env, monitor);
        line("raw monitor: enter=%d enter=%d exit=%d wait=%d notify=%d notifyAll=%d exit=%d exit=%d destroy=%d",
             a, b, c, d, e, f, g, h, k);
    }

    {
        void *mine = NULL;
        void *theirs = (void *)1;
        int set = (int)(*agent_env)->SetThreadLocalStorage(agent_env, NULL, &storage_cookie);
        int get = (int)(*agent_env)->GetThreadLocalStorage(agent_env, NULL, &mine);
        int other = (int)(*agent_env)->GetThreadLocalStorage(agent_env, worker, &theirs);
        line("thread storage: set=%d get=%d same=%d worker=%d null=%d", set, get,
             mine == (void *)&storage_cookie, other, theirs == NULL);
    }
    thread_row(env, "worker", worker, app);
    thread_row(env, "unstarted", unstarted, app);

    memset(&info, 0, sizeof info);
    if ((*agent_env)->GetThreadInfo(agent_env, worker, &info) == JVMTI_ERROR_NONE) {
        jvmtiThreadGroupInfo group;
        dealloc(info.name);
        group_row(info.thread_group);
        memset(&group, 0, sizeof group);
        if ((*agent_env)->GetThreadGroupInfo(agent_env, info.thread_group, &group) == JVMTI_ERROR_NONE) {
            dealloc(group.name);
            group_row(group.parent);
        }
    }

    err = (int)(*agent_env)->GetTopThreadGroups(agent_env, &count, &groups);
    if (err == JVMTI_ERROR_NONE && count > 0) {
        jvmtiThreadGroupInfo top;
        memset(&top, 0, sizeof top);
        (*agent_env)->GetThreadGroupInfo(agent_env, groups[0], &top);
        group_name(groups[0], name, sizeof name);
        group_name(top.parent, parent, sizeof parent);
        dealloc(top.name);
        line("top groups: err=%d count=%d first=%s parent=%s", err, (int)count, name, parent);
    } else {
        line("top groups: err=%d count=%d", err, (int)count);
    }
    dealloc(groups);

    for (i = 0; i < (*env)->GetArrayLength(env, classes); i++) {
        jclass klass = (jclass)(*env)->GetObjectArrayElement(env, classes, i);
        class_row(env, klass, app);
    }

    fields_rows("Pair", pair);
    created = (*env)->GetStaticFieldID(env, pair, "created", "I");
    count_field = (*env)->GetFieldID(env, pair, "count", "I");
    if (created == NULL || count_field == NULL) {
        (*env)->ExceptionClear(env);
        line("jni ids: missing");
    } else {
        field_name_of(pair, created, created_name, sizeof created_name);
        field_name_of(pair, count_field, count_name, sizeof count_name);
        line("jni ids: static created=%d name=%s instance count name=%s",
             (int)(*env)->GetStaticIntField(env, pair, created), created_name, count_name);
    }
    fields_rows("Inner", inner);

    method_row(env, pair, "sum", "(IJLjava/lang/Object;)J", 1);
    method_row(env, pair, "run", "()V", 0);

    thread_row(env, "NULL again", NULL, app);
    return (*env)->NewStringUTF(env, out);
}

JNIEXPORT jstring JNICALL
Java_L1W52JvmtiThreadAndClassFacts_threadEvents(JNIEnv *env, jclass self)
{
    char text[600];
    (void)self;
    snprintf(text, sizeof text, "thread events:%s\n", thread_events);
    return (*env)->NewStringUTF(env, text);
}
