/*
 * Native shim of tools/probes/interp/L1/L1W63JvmtiAgentFunctions.java: the
 * startup agent adds every capability it is offered, and the natives make
 * the calls the probe prints. The Rust port
 * (tools/probes/interp/L1/rust-ports/L1W63JvmtiAgentFunctions) makes the same
 * calls in the same order and prints the same lines.
 */
#include <jvmti.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define NO_ENV "no JVMTI env (load the shim with -agentpath too)\n"

static jvmtiEnv *agent;
static JavaVM *vm_handle;
static char onload_out[4096];

static const char *CAPS[45] = {
    "can_tag_objects", "can_generate_field_modification_events", "can_generate_field_access_events",
    "can_get_bytecodes", "can_get_synthetic_attribute", "can_get_owned_monitor_info",
    "can_get_current_contended_monitor", "can_get_monitor_info", "can_pop_frame",
    "can_redefine_classes", "can_signal_thread", "can_get_source_file_name", "can_get_line_numbers",
    "can_get_source_debug_extension", "can_access_local_variables",
    "can_maintain_original_method_order", "can_generate_single_step_events",
    "can_generate_exception_events", "can_generate_frame_pop_events",
    "can_generate_breakpoint_events", "can_suspend", "can_redefine_any_class",
    "can_get_current_thread_cpu_time", "can_get_thread_cpu_time",
    "can_generate_method_entry_events", "can_generate_method_exit_events",
    "can_generate_all_class_hook_events", "can_generate_compiled_method_load_events",
    "can_generate_monitor_events", "can_generate_vm_object_alloc_events",
    "can_generate_native_method_bind_events", "can_generate_garbage_collection_events",
    "can_generate_object_free_events", "can_force_early_return",
    "can_get_owned_monitor_stack_depth_info", "can_get_constant_pool",
    "can_set_native_method_prefix", "can_retransform_classes", "can_retransform_any_class",
    "can_generate_resource_exhaustion_heap_events",
    "can_generate_resource_exhaustion_threads_events", "can_generate_early_vmstart",
    "can_generate_early_class_hook_events", "can_generate_sampled_object_alloc_events",
    "can_support_virtual_threads",
};

/* The capability bits as the Rust port reads them: bit i of word i / 32. */
static void names(const jvmtiCapabilities *c, char *out, size_t size) {
    const unsigned int *w = (const unsigned int *)c;
    int i;
    out[0] = 0;
    for (i = 0; i < 45; i++) {
        if (w[i / 32] & (1u << (i % 32))) {
            if (out[0]) strncat(out, " ", size - strlen(out) - 1);
            strncat(out, CAPS[i], size - strlen(out) - 1);
        }
    }
}

static void append(char *buf, size_t size, const char *fmt, ...) {
    va_list ap;
    size_t len = strlen(buf);
    va_start(ap, fmt);
    vsnprintf(buf + len, size - len, fmt, ap);
    va_end(ap);
}

JNIEXPORT jint JNICALL Agent_OnLoad(JavaVM *vm, char *options, void *reserved) {
    jvmtiCapabilities p;
    char list[4096];
    jint n = -1;
    jvmtiExtensionFunctionInfo *fs = NULL;
    jvmtiError e;
    vm_handle = vm;
    if ((*vm)->GetEnv(vm, (void **)&agent, JVMTI_VERSION_1_2) != JNI_OK) return 1;
    memset(&p, 0, sizeof p);
    e = (*agent)->GetPotentialCapabilities(agent, &p);
    names(&p, list, sizeof list);
    append(onload_out, sizeof onload_out, "onload potential: err=%d %s\n", e, list);
    append(onload_out, sizeof onload_out, "onload add all: err=%d\n", (*agent)->AddCapabilities(agent, &p));
    append(onload_out, sizeof onload_out, "onload SetHeapSamplingInterval 1024: err=%d\n",
           (*agent)->SetHeapSamplingInterval(agent, 1024));
    e = (*agent)->GetExtensionFunctions(agent, &n, &fs);
    append(onload_out, sizeof onload_out, "onload GetExtensionFunctions: err=%d count=%d\n", e, n);
    return 0;
}

#define NEED_AGENT(env) if (agent == NULL) return (*env)->NewStringUTF(env, NO_ENV)

JNIEXPORT jstring JNICALL Java_L1W63JvmtiAgentFunctions_onload(JNIEnv *env, jclass cls) {
    NEED_AGENT(env);
    return (*env)->NewStringUTF(env, onload_out);
}

JNIEXPORT jstring JNICALL Java_L1W63JvmtiAgentFunctions_caps(JNIEnv *env, jclass cls) {
    char out[8192] = "", list[4096];
    jvmtiCapabilities held, p;
    jvmtiEnv *fresh;
    jvmtiError e;
    NEED_AGENT(env);
    memset(&held, 0, sizeof held);
    e = (*agent)->GetCapabilities(agent, &held);
    names(&held, list, sizeof list);
    append(out, sizeof out, "caps held: err=%d %s\n", e, list);
    if ((*vm_handle)->GetEnv(vm_handle, (void **)&fresh, JVMTI_VERSION_1_2) == JNI_OK) {
        memset(&p, 0, sizeof p);
        e = (*fresh)->GetPotentialCapabilities(fresh, &p);
        names(&p, list, sizeof list);
        append(out, sizeof out, "caps live: err=%d %s\n", e, list);
    } else {
        append(out, sizeof out, "caps live: no env\n");
    }
    return (*env)->NewStringUTF(env, out);
}

/* ---- extensions ---------------------------------------------------------- */

static void params(const jvmtiParamInfo *p, jint n, char *out, size_t size) {
    jint i;
    out[0] = 0;
    for (i = 0; i < n; i++) {
        append(out, size, "%s%s:%d:%d:%d", i ? "," : "", p[i].name, p[i].kind, p[i].base_type, p[i].null_ok);
    }
}

static void JNICALL ext_event_callback(jvmtiEnv *jvmti, ...) {}

static char *thread_name(jthread t) {
    static char buf[256];
    jvmtiThreadInfo info;
    jvmtiError e;
    if (t == NULL) return "null";
    memset(&info, 0, sizeof info);
    e = (*agent)->GetThreadInfo(agent, t, &info);
    if (e != JVMTI_ERROR_NONE) {
        snprintf(buf, sizeof buf, "?%d", e);
        return buf;
    }
    snprintf(buf, sizeof buf, "%s", info.name);
    (*agent)->Deallocate(agent, (unsigned char *)info.name);
    return buf;
}

static jboolean is_virtual(JNIEnv *env, jthread t) {
    jclass cls = (*env)->GetObjectClass(env, t);
    jmethodID mid = (*env)->GetMethodID(env, cls, "isVirtual", "()Z");
    return (*env)->CallBooleanMethodA(env, t, mid, NULL);
}

typedef jvmtiError (JNICALL *ExtCall2)(jvmtiEnv *, jthread, jthread *);

static jvmtiExtensionFunction find_ext(jvmtiExtensionFunctionInfo *fs, jint n, const char *id) {
    jint i;
    for (i = 0; i < n; i++) {
        if (strcmp(fs[i].id, id) == 0) return fs[i].func;
    }
    return NULL;
}

JNIEXPORT jstring JNICALL Java_L1W63JvmtiAgentFunctions_extensions(JNIEnv *env, jclass cls, jthread platform, jthread parked) {
    char out[16384] = "", p[1024];
    jint n = 0, i, k;
    jvmtiExtensionFunctionInfo *fs = NULL;
    jvmtiExtensionEventInfo *evs = NULL;
    jvmtiExtensionFunction unloading, gv, gc;
    jvmtiError e;
    int indices[5] = {46, 47, 48, 49, 50};
    NEED_AGENT(env);
    e = (*agent)->GetExtensionFunctions(agent, &n, &fs);
    append(out, sizeof out, "ext functions: err=%d count=%d\n", e, n);
    for (i = 0; i < n; i++) {
        char errors[256] = "";
        for (k = 0; k < fs[i].error_count; k++) append(errors, sizeof errors, "%s%d", k ? "," : "", fs[i].errors[k]);
        params(fs[i].params, fs[i].param_count, p, sizeof p);
        append(out, sizeof out, "ext fn %s [%s] params=%s errors=%s\n", fs[i].id, fs[i].short_description, p, errors);
    }
    {
        jint m = 0;
        e = (*agent)->GetExtensionEvents(agent, &m, &evs);
        append(out, sizeof out, "ext events: err=%d count=%d\n", e, m);
        for (i = 0; i < m; i++) {
            params(evs[i].params, evs[i].param_count, p, sizeof p);
            append(out, sizeof out, "ext ev %d %s [%s] params=%s\n", evs[i].extension_event_index, evs[i].id,
                   evs[i].short_description, p);
        }
    }
    unloading = find_ext(fs, n, "com.sun.hotspot.functions.IsClassUnloadingEnabled");
    if (unloading != NULL) {
        jboolean v = 7;
        e = unloading(agent, &v);
        append(out, sizeof out, "IsClassUnloadingEnabled: err=%d value=%d\n", e, v);
    }
    gv = find_ext(fs, n, "com.sun.hotspot.functions.GetVirtualThread");
    gc = find_ext(fs, n, "com.sun.hotspot.functions.GetCarrierThread");
    if (gv != NULL && gc != NULL) {
        jthread t = (jthread)1;
        e = gv(agent, platform, &t);
        append(out, sizeof out, "GetVirtualThread platform: err=%d vthread=%s\n", e, thread_name(t));
        t = (jthread)1;
        e = gc(agent, parked, &t);
        append(out, sizeof out, "GetCarrierThread parked: err=%d carrier=%s\n", e, thread_name(t));
        t = (jthread)1;
        e = gc(agent, platform, &t);
        append(out, sizeof out, "GetCarrierThread platform: err=%d\n", e);
        t = (jthread)1;
        e = gv(agent, parked, &t);
        append(out, sizeof out, "GetVirtualThread parked: err=%d\n", e);
        e = gv(agent, platform, NULL);
        append(out, sizeof out, "GetVirtualThread null out: err=%d\n", e);
        e = gc(agent, NULL, &t);
        append(out, sizeof out, "GetCarrierThread null thread: err=%d\n", e);
    }
    for (i = 0; i < 5; i++) {
        e = (*agent)->SetExtensionEventCallback(agent, indices[i], (jvmtiExtensionEvent)ext_event_callback);
        append(out, sizeof out, "SetExtensionEventCallback %d: err=%d\n", indices[i], e);
        (*agent)->SetExtensionEventCallback(agent, indices[i], NULL);
    }
    e = (*agent)->GetExtensionFunctions(agent, NULL, &fs);
    append(out, sizeof out, "GetExtensionFunctions null count: err=%d\n", e);
    e = (*agent)->GetExtensionEvents(agent, &n, NULL);
    append(out, sizeof out, "GetExtensionEvents null array: err=%d\n", e);
    return (*env)->NewStringUTF(env, out);
}

JNIEXPORT jstring JNICALL Java_L1W63JvmtiAgentFunctions_mounted(JNIEnv *env, jclass cls) {
    char out[1024] = "";
    jint n = 0;
    jvmtiExtensionFunctionInfo *fs = NULL;
    jvmtiExtensionFunction gv, gc;
    jthread cur = NULL, carrier = NULL;
    jvmtiError e;
    NEED_AGENT(env);
    (*agent)->GetExtensionFunctions(agent, &n, &fs);
    gv = find_ext(fs, n, "com.sun.hotspot.functions.GetVirtualThread");
    gc = find_ext(fs, n, "com.sun.hotspot.functions.GetCarrierThread");
    if (gv == NULL || gc == NULL) return (*env)->NewStringUTF(env, "mounted: no extension functions\n");
    (*agent)->GetCurrentThread(agent, &cur);
    e = gc(agent, cur, &carrier);
    append(out, sizeof out, "mounted GetCarrierThread: err=%d carrier=%s carrier-virtual=%d\n", e,
           carrier == NULL ? "null" : "thread", carrier != NULL && is_virtual(env, carrier));
    if (carrier != NULL) {
        jthread back = NULL;
        e = gv(agent, carrier, &back);
        append(out, sizeof out, "mounted GetVirtualThread carrier: err=%d same=%d\n", e,
               (*env)->IsSameObject(env, back, cur));
    }
    return (*env)->NewStringUTF(env, out);
}

/* ---- SetVerboseFlag, the JNI function table ------------------------------- */

JNIEXPORT jint JNICALL Java_L1W63JvmtiAgentFunctions_verbose(JNIEnv *env, jclass cls, jint flag, jboolean value) {
    if (agent == NULL) return -1;
    return (*agent)->SetVerboseFlag(agent, (jvmtiVerboseFlag)flag, value);
}

static jniNativeInterface saved;
static jniNativeInterface hooked;
static int version_calls;

static jint JNICALL counting_get_version(JNIEnv *env) {
    version_calls++;
    return saved.GetVersion(env);
}

JNIEXPORT jstring JNICALL Java_L1W63JvmtiAgentFunctions_jniTable(JNIEnv *env, jclass cls) {
    char out[1024] = "";
    jniNativeInterface *table = NULL;
    jvmtiError e;
    int same = 1;
    NEED_AGENT(env);
    e = (*agent)->GetJNIFunctionTable(agent, &table);
    if (e == JVMTI_ERROR_NONE) {
        same = memcmp(table, *env, sizeof(jniNativeInterface)) == 0;
        saved = *table;
    }
    append(out, sizeof out, "jni get: err=%d same=%d copy=%d\n", e, same, table != *env);
    append(out, sizeof out, "jni get null: err=%d\n", (*agent)->GetJNIFunctionTable(agent, NULL));
    append(out, sizeof out, "jni set null: err=%d\n", (*agent)->SetJNIFunctionTable(agent, NULL));
    hooked = saved;
    hooked.GetVersion = counting_get_version;
    e = (*agent)->SetJNIFunctionTable(agent, &hooked);
    append(out, sizeof out, "jni set: err=%d\n", e);
    return (*env)->NewStringUTF(env, out);
}

JNIEXPORT jstring JNICALL Java_L1W63JvmtiAgentFunctions_jniVersion(JNIEnv *env, jclass cls) {
    char out[256];
    jint version, hooked_calls;
    jvmtiError e;
    NEED_AGENT(env);
    version = (*env)->GetVersion(env);
    hooked_calls = version_calls;
    e = (*agent)->SetJNIFunctionTable(agent, &saved);
    (*env)->GetVersion(env);
    snprintf(out, sizeof out, "jni version: %#x hooked-calls=%d restore err=%d after-restore=%d\n", version,
             hooked_calls, e, version_calls);
    return (*env)->NewStringUTF(env, out);
}

/* ---- class-file attributes ---------------------------------------------------- */

JNIEXPORT jstring JNICALL Java_L1W63JvmtiAgentFunctions_sourceDebugExtension(JNIEnv *env, jclass cls, jclass k) {
    char *text = NULL;
    char out[4096];
    jvmtiError e;
    NEED_AGENT(env);
    e = (*agent)->GetSourceDebugExtension(agent, k, &text);
    if (e != JVMTI_ERROR_NONE) {
        snprintf(out, sizeof out, "!%d", e);
        return (*env)->NewStringUTF(env, out);
    }
    snprintf(out, sizeof out, "=%s", text);
    (*agent)->Deallocate(agent, (unsigned char *)text);
    return (*env)->NewStringUTF(env, out);
}

JNIEXPORT jstring JNICALL Java_L1W63JvmtiAgentFunctions_constantPool(JNIEnv *env, jclass cls, jclass k) {
    jint count = 0, len = 0, i;
    unsigned char *bytes = NULL;
    char *out;
    jstring s;
    jvmtiError e;
    NEED_AGENT(env);
    e = (*agent)->GetConstantPool(agent, k, &count, &len, &bytes);
    if (e != JVMTI_ERROR_NONE) {
        char buf[32];
        snprintf(buf, sizeof buf, "%d", e);
        return (*env)->NewStringUTF(env, buf);
    }
    out = malloc(64 + 2 * (size_t)len);
    sprintf(out, "0 %d ", count);
    for (i = 0; i < len; i++) sprintf(out + strlen(out), "%02x", bytes[i]);
    (*agent)->Deallocate(agent, bytes);
    s = (*env)->NewStringUTF(env, out);
    free(out);
    return s;
}

/* ---- monitors, interrupt, stop --------------------------------------------- */

static int cmp_str(const void *a, const void *b) { return strcmp(*(char *const *)a, *(char *const *)b); }

static void sorted_names(jthread *list, jint n, char *out, size_t size) {
    char *names_buf[64];
    jint i;
    out[0] = 0;
    for (i = 0; i < n && i < 64; i++) names_buf[i] = strdup(thread_name(list[i]));
    qsort(names_buf, (size_t)(n < 64 ? n : 64), sizeof(char *), cmp_str);
    for (i = 0; i < n && i < 64; i++) {
        append(out, size, "%s%s", i ? "," : "", names_buf[i]);
        free(names_buf[i]);
    }
    if (list != NULL) (*agent)->Deallocate(agent, (unsigned char *)list);
}

JNIEXPORT jstring JNICALL Java_L1W63JvmtiAgentFunctions_monitorUsage(JNIEnv *env, jclass cls, jobject o) {
    jvmtiMonitorUsage u;
    char owner[256], waiters[1024], notify[1024], out[4096];
    jvmtiError e;
    NEED_AGENT(env);
    memset(&u, 0, sizeof u);
    u.entry_count = u.waiter_count = u.notify_waiter_count = -1;
    e = (*agent)->GetObjectMonitorUsage(agent, o, &u);
    if (e != JVMTI_ERROR_NONE) {
        snprintf(out, sizeof out, "err=%d", e);
        return (*env)->NewStringUTF(env, out);
    }
    snprintf(owner, sizeof owner, "%s", thread_name(u.owner));
    sorted_names(u.waiters, u.waiter_count, waiters, sizeof waiters);
    sorted_names(u.notify_waiters, u.notify_waiter_count, notify, sizeof notify);
    snprintf(out, sizeof out, "err=0 owner=%s entry_count=%d waiters=%d[%s] notify_waiters=%d[%s]", owner,
             u.entry_count, u.waiter_count, waiters, u.notify_waiter_count, notify);
    return (*env)->NewStringUTF(env, out);
}

JNIEXPORT jint JNICALL Java_L1W63JvmtiAgentFunctions_interrupt(JNIEnv *env, jclass cls, jthread t) {
    return agent == NULL ? -1 : (*agent)->InterruptThread(agent, t);
}

JNIEXPORT jint JNICALL Java_L1W63JvmtiAgentFunctions_interruptString(JNIEnv *env, jclass cls, jobject t) {
    return agent == NULL ? -1 : (*agent)->InterruptThread(agent, (jthread)t);
}

JNIEXPORT jint JNICALL Java_L1W63JvmtiAgentFunctions_stop(JNIEnv *env, jclass cls, jthread t, jobject ex) {
    return agent == NULL ? -1 : (*agent)->StopThread(agent, t, ex);
}

/* ---- RunAgentThread ------------------------------------------------------------ */

static jrawMonitorID lock;
static volatile int done;
static jthread agent_thread;
static char proc_out[512];

static void JNICALL agent_proc(jvmtiEnv *jvmti, JNIEnv *env, void *arg) {
    jvmtiThreadInfo info;
    jthread cur = NULL;
    jclass thread_cls;
    jmethodID mid;
    jobject current;
    jint state = 0;
    char name[256];
    jvmtiError e;
    memset(&info, 0, sizeof info);
    e = (*jvmti)->GetThreadInfo(jvmti, NULL, &info);
    if (e == JVMTI_ERROR_NONE) {
        snprintf(name, sizeof name, "%s", info.name);
        (*jvmti)->Deallocate(jvmti, (unsigned char *)info.name);
    } else {
        snprintf(name, sizeof name, "?%d", e);
    }
    (*jvmti)->GetCurrentThread(jvmti, &cur);
    thread_cls = (*env)->FindClass(env, "java/lang/Thread");
    mid = (*env)->GetStaticMethodID(env, thread_cls, "currentThread", "()Ljava/lang/Thread;");
    current = (*env)->CallStaticObjectMethodA(env, thread_cls, mid, NULL);
    (*jvmti)->GetThreadState(jvmti, NULL, &state);
    snprintf(proc_out, sizeof proc_out,
             "agent proc: name=%s priority=%d daemon=%d arg=%d current=%d currentThread=%d alive=%d runnable=%d\n",
             name, info.priority, info.is_daemon, arg == (void *)0x1234, (*env)->IsSameObject(env, cur, agent_thread),
             (*env)->IsSameObject(env, current, agent_thread), state & 1, (state & 4) != 0);
    (*jvmti)->RawMonitorEnter(jvmti, lock);
    done = 1;
    (*jvmti)->RawMonitorNotifyAll(jvmti, lock);
    (*jvmti)->RawMonitorExit(jvmti, lock);
}

static void JNICALL noop_proc(jvmtiEnv *jvmti, JNIEnv *env, void *arg) {}

JNIEXPORT jstring JNICALL Java_L1W63JvmtiAgentFunctions_runAgentThread(JNIEnv *env, jclass cls, jthread t, jint priority) {
    char out[1024] = "";
    jvmtiError e;
    int rounds = 0;
    NEED_AGENT(env);
    if (lock == NULL) (*agent)->CreateRawMonitor(agent, "l1w63", &lock);
    agent_thread = (*env)->NewGlobalRef(env, t);
    e = (*agent)->RunAgentThread(agent, t, agent_proc, (void *)0x1234, priority);
    append(out, sizeof out, "RunAgentThread: err=%d\n", e);
    if (e == JVMTI_ERROR_NONE) {
        (*agent)->RawMonitorEnter(agent, lock);
        while (!done && rounds < 50) {
            (*agent)->RawMonitorWait(agent, lock, 100);
            rounds++;
        }
        (*agent)->RawMonitorExit(agent, lock);
        append(out, sizeof out, "%s", done ? proc_out : "agent proc: did not run\n");
    }
    return (*env)->NewStringUTF(env, out);
}

JNIEXPORT jstring JNICALL Java_L1W63JvmtiAgentFunctions_runAgentThreadErrors(JNIEnv *env, jclass cls, jthread started, jthread fresh) {
    char out[2048] = "";
    jobject not_thread;
    NEED_AGENT(env);
    not_thread = (*env)->NewStringUTF(env, "x");
    append(out, sizeof out, "RunAgentThread null proc: err=%d\n", (*agent)->RunAgentThread(agent, fresh, NULL, NULL, 5));
    append(out, sizeof out, "RunAgentThread priority 0: err=%d\n", (*agent)->RunAgentThread(agent, fresh, noop_proc, NULL, 0));
    append(out, sizeof out, "RunAgentThread priority 11: err=%d\n", (*agent)->RunAgentThread(agent, fresh, noop_proc, NULL, 11));
    append(out, sizeof out, "RunAgentThread null thread: err=%d\n", (*agent)->RunAgentThread(agent, NULL, noop_proc, NULL, 5));
    append(out, sizeof out, "RunAgentThread not a thread: err=%d\n",
           (*agent)->RunAgentThread(agent, (jthread)not_thread, noop_proc, NULL, 5));
    append(out, sizeof out, "RunAgentThread terminated thread: err=%d\n",
           (*agent)->RunAgentThread(agent, started, noop_proc, NULL, 5));
    return (*env)->NewStringUTF(env, out);
}

/* ---- modules -------------------------------------------------------------------- */

JNIEXPORT jstring JNICALL Java_L1W63JvmtiAgentFunctions_modules(JNIEnv *env, jclass cls, jobject base, jobject logging,
                                                                jobject unnamed, jclass service, jclass impl,
                                                                jobject not_module) {
    char out[4096] = "";
    jint n = 0, i;
    jobject *list = NULL;
    jobject m;
    jboolean v;
    jvmtiError e;
    int has_base = 0, has_logging = 0, has_unnamed = 0;
    const char *pkgs[3] = {"java/lang", "java/util/logging", "no/such"};
    jobject wants[3];
    NEED_AGENT(env);
    wants[0] = base;
    wants[1] = logging;
    wants[2] = NULL;
    e = (*agent)->GetAllModules(agent, &n, &list);
    for (i = 0; i < n; i++) {
        has_base |= (*env)->IsSameObject(env, list[i], base);
        has_logging |= (*env)->IsSameObject(env, list[i], logging);
        has_unnamed |= (*env)->IsSameObject(env, list[i], unnamed);
    }
    append(out, sizeof out, "GetAllModules: err=%d many=%d base=%d logging=%d app-unnamed=%d\n", e, n > 20,
           has_base, has_logging, has_unnamed);
    (*agent)->Deallocate(agent, (unsigned char *)list);
    append(out, sizeof out, "GetAllModules null: err=%d\n", (*agent)->GetAllModules(agent, NULL, &list));
    for (i = 0; i < 3; i++) {
        m = (jobject)1;
        e = (*agent)->GetNamedModule(agent, NULL, pkgs[i], &m);
        if (m == NULL) {
            append(out, sizeof out, "GetNamedModule boot %s: err=%d null\n", pkgs[i], e);
        } else {
            append(out, sizeof out, "GetNamedModule boot %s: err=%d same=%d\n", pkgs[i], e,
                   (*env)->IsSameObject(env, m, wants[i]));
        }
    }
    append(out, sizeof out, "GetNamedModule null package: err=%d\n", (*agent)->GetNamedModule(agent, NULL, NULL, &m));
    append(out, sizeof out, "GetNamedModule not a loader: err=%d\n", (*agent)->GetNamedModule(agent, not_module, "java/lang", &m));
    append(out, sizeof out, "AddModuleReads: err=%d\n", (*agent)->AddModuleReads(agent, logging, unnamed));
    append(out, sizeof out, "AddModuleExports dotted: err=%d\n", (*agent)->AddModuleExports(agent, base, "jdk.internal.misc", unnamed));
    append(out, sizeof out, "AddModuleExports slashed: err=%d\n", (*agent)->AddModuleExports(agent, base, "jdk/internal/misc", unnamed));
    append(out, sizeof out, "AddModuleExports not in module: err=%d\n", (*agent)->AddModuleExports(agent, base, "no.such", unnamed));
    append(out, sizeof out, "AddModuleOpens: err=%d\n", (*agent)->AddModuleOpens(agent, base, "jdk.internal.reflect", unnamed));
    append(out, sizeof out, "AddModuleUses: err=%d\n", (*agent)->AddModuleUses(agent, logging, service));
    append(out, sizeof out, "AddModuleProvides: err=%d\n", (*agent)->AddModuleProvides(agent, logging, service, impl));
    append(out, sizeof out, "AddModuleReads not a module: err=%d\n", (*agent)->AddModuleReads(agent, not_module, unnamed));
    append(out, sizeof out, "AddModuleReads null to: err=%d\n", (*agent)->AddModuleReads(agent, logging, NULL));
    append(out, sizeof out, "AddModuleUses not a class: err=%d\n", (*agent)->AddModuleUses(agent, logging, (jclass)not_module));
    v = 7;
    e = (*agent)->IsModifiableModule(agent, base, &v);
    append(out, sizeof out, "IsModifiableModule base: err=%d value=%d\n", e, v);
    v = 7;
    e = (*agent)->IsModifiableModule(agent, unnamed, &v);
    append(out, sizeof out, "IsModifiableModule unnamed: err=%d value=%d\n", e, v);
    append(out, sizeof out, "IsModifiableModule not a module: err=%d\n", (*agent)->IsModifiableModule(agent, not_module, &v));
    return (*env)->NewStringUTF(env, out);
}

/* ---- virtual threads, sampling, prefixes ---------------------------------------- */

static int suspended(jthread t) {
    jint state = 0;
    jvmtiError e = (*agent)->GetThreadState(agent, t, &state);
    if (e != JVMTI_ERROR_NONE) return -e;
    return (state & JVMTI_THREAD_STATE_SUSPENDED) != 0;
}

static void vt_row(JNIEnv *env, jobjectArray vts, const char *label, char *out, size_t size) {
    jint n = (*env)->GetArrayLength(env, vts), i;
    append(out, size, "%s:", label);
    for (i = 0; i < n; i++) append(out, size, " vt%d=%d", i, suspended((*env)->GetObjectArrayElement(env, vts, i)));
    append(out, size, "\n");
}

JNIEXPORT jstring JNICALL Java_L1W63JvmtiAgentFunctions_suspendAllVirtual(JNIEnv *env, jclass cls, jobjectArray vts, jthread except) {
    char out[2048] = "";
    jthread list[1];
    jvmtiError e;
    NEED_AGENT(env);
    list[0] = except;
    append(out, sizeof out, "SuspendAll negative: err=%d\n", (*agent)->SuspendAllVirtualThreads(agent, -1, NULL));
    append(out, sizeof out, "SuspendAll null list: err=%d\n", (*agent)->SuspendAllVirtualThreads(agent, 1, NULL));
    e = (*agent)->SuspendAllVirtualThreads(agent, 1, list);
    append(out, sizeof out, "SuspendAllVirtualThreads except vt1: err=%d\n", e);
    vt_row(env, vts, "suspended", out, sizeof out);
    e = (*agent)->ResumeAllVirtualThreads(agent, 0, NULL);
    append(out, sizeof out, "ResumeAllVirtualThreads: err=%d\n", e);
    vt_row(env, vts, "resumed", out, sizeof out);
    return (*env)->NewStringUTF(env, out);
}

JNIEXPORT jint JNICALL Java_L1W63JvmtiAgentFunctions_sampling(JNIEnv *env, jclass cls, jint n) {
    return agent == NULL ? -1 : (*agent)->SetHeapSamplingInterval(agent, n);
}

JNIEXPORT jint JNICALL Java_L1W63JvmtiAgentFunctions_prefix(JNIEnv *env, jclass cls, jstring p) {
    const char *text;
    jint e;
    if (agent == NULL) return -1;
    if (p == NULL) return (*agent)->SetNativeMethodPrefix(agent, NULL);
    text = (*env)->GetStringUTFChars(env, p, NULL);
    e = (*agent)->SetNativeMethodPrefix(agent, text);
    (*env)->ReleaseStringUTFChars(env, p, text);
    return e;
}

JNIEXPORT jint JNICALL Java_L1W63JvmtiAgentFunctions_prefixes(JNIEnv *env, jclass cls, jobjectArray arr) {
    char *texts[16];
    jint n, i, e;
    if (agent == NULL) return -1;
    if (arr == NULL) return (*agent)->SetNativeMethodPrefixes(agent, -1, NULL);
    n = (*env)->GetArrayLength(env, arr);
    for (i = 0; i < n && i < 16; i++) {
        jstring s = (jstring)(*env)->GetObjectArrayElement(env, arr, i);
        const char *t = (*env)->GetStringUTFChars(env, s, NULL);
        texts[i] = strdup(t);
        (*env)->ReleaseStringUTFChars(env, s, t);
    }
    e = (*agent)->SetNativeMethodPrefixes(agent, n, texts);
    for (i = 0; i < n && i < 16; i++) free(texts[i]);
    return e;
}

JNIEXPORT jint JNICALL Java_L1W63JvmtiAgentFunctions_twice(JNIEnv *env, jclass cls, jint x) { return x * 2; }

JNIEXPORT jint JNICALL Java_L1W63JvmtiAgentFunctions_thrice(JNIEnv *env, jclass cls, jint x) { return x * 3; }
