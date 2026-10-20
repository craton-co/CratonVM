/*
 * Native shim of tools/probes/interp/L1/L1W63JvmtiEvents.java: the startup
 * agent adds every capability it is offered and sets a recording callback for
 * every event the probe enables; `drain` hands the recorded lines back. The
 * Rust port (tools/probes/interp/L1/rust-ports/L1W63JvmtiEvents) makes the
 * same calls in the same order and prints the same lines.
 */
#include <jvmti.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define NO_ENV "no JVMTI env (load the shim with -agentpath too)\n"

static jvmtiEnv *agent;
static jrawMonitorID lines_lock;
static char *lines[1024];
static int nlines;
static volatile int gc_starts, gc_finishes, sampled, generated;

static void record(const char *fmt, ...) {
    char buf[512];
    va_list ap;
    va_start(ap, fmt);
    vsnprintf(buf, sizeof buf, fmt, ap);
    va_end(ap);
    (*agent)->RawMonitorEnter(agent, lines_lock);
    if (nlines < 1024) lines[nlines++] = strdup(buf);
    (*agent)->RawMonitorExit(agent, lines_lock);
}

static void append(char *buf, size_t size, const char *fmt, ...) {
    va_list ap;
    size_t len = strlen(buf);
    va_start(ap, fmt);
    vsnprintf(buf + len, size - len, fmt, ap);
    va_end(ap);
}

static void thread_name(jvmtiEnv *jvmti, jthread t, char *out, size_t size) {
    jvmtiThreadInfo info;
    memset(&info, 0, sizeof info);
    if (t == NULL || (*jvmti)->GetThreadInfo(jvmti, t, &info) != JVMTI_ERROR_NONE) {
        snprintf(out, size, "?");
        return;
    }
    snprintf(out, size, "%s", info.name);
    (*jvmti)->Deallocate(jvmti, (unsigned char *)info.name);
}

static void method_name(jvmtiEnv *jvmti, jmethodID m, char *out, size_t size) {
    char *name = NULL;
    if ((*jvmti)->GetMethodName(jvmti, m, &name, NULL, NULL) != JVMTI_ERROR_NONE) {
        snprintf(out, size, "?");
        return;
    }
    snprintf(out, size, "%s", name);
    (*jvmti)->Deallocate(jvmti, (unsigned char *)name);
}

static void field_name(jvmtiEnv *jvmti, jclass klass, jfieldID field, char *out, size_t size) {
    char *name = NULL;
    if ((*jvmti)->GetFieldName(jvmti, klass, field, &name, NULL, NULL) != JVMTI_ERROR_NONE) {
        snprintf(out, size, "?");
        return;
    }
    snprintf(out, size, "%s", name);
    (*jvmti)->Deallocate(jvmti, (unsigned char *)name);
}

static void signature_of(jvmtiEnv *jvmti, jclass klass, char *out, size_t size) {
    char *sig = NULL;
    if (klass == NULL) {
        snprintf(out, size, "null");
        return;
    }
    if ((*jvmti)->GetClassSignature(jvmti, klass, &sig, NULL) != JVMTI_ERROR_NONE) {
        snprintf(out, size, "?");
        return;
    }
    snprintf(out, size, "%s", sig);
    (*jvmti)->Deallocate(jvmti, (unsigned char *)sig);
}

static jlong tag_of(jvmtiEnv *jvmti, jobject o) {
    jlong tag = 0;
    (*jvmti)->GetTag(jvmti, o, &tag);
    return tag;
}

/* ---- callbacks ------------------------------------------------------------ */

static void JNICALL on_field_access(jvmtiEnv *jvmti, JNIEnv *jni, jthread t, jmethodID m, jlocation loc, jclass klass,
                                    jobject obj, jfieldID field) {
    char f[128], mn[128], tn[128];
    field_name(jvmti, klass, field, f, sizeof f);
    method_name(jvmti, m, mn, sizeof mn);
    thread_name(jvmti, t, tn, sizeof tn);
    record("access %s in %s at %lld by %s object=%s", f, mn, (long long)loc, tn, obj == NULL ? "null" : "set");
}

static void JNICALL on_field_modification(jvmtiEnv *jvmti, JNIEnv *jni, jthread t, jmethodID m, jlocation loc,
                                          jclass klass, jobject obj, jfieldID field, char sig, jvalue value) {
    char f[128], mn[128], tn[128], shown[64];
    field_name(jvmti, klass, field, f, sizeof f);
    method_name(jvmti, m, mn, sizeof mn);
    thread_name(jvmti, t, tn, sizeof tn);
    switch (sig) {
    case 'I': snprintf(shown, sizeof shown, "%d", value.i); break;
    case 'J': snprintf(shown, sizeof shown, "%lld", (long long)value.j); break;
    case 'L': snprintf(shown, sizeof shown, "%s", value.l == NULL ? "null" : "object"); break;
    default: snprintf(shown, sizeof shown, "%c:%#llx", sig, (unsigned long long)value.j); break;
    }
    record("modify %s in %s at %lld by %s object=%s sig=%c value=%s", f, mn, (long long)loc, tn,
           obj == NULL ? "null" : "set", sig, shown);
}

static jint JNICALL replacement(JNIEnv *env, jclass cls) { return 77; }
static jint JNICALL registered_impl(JNIEnv *env, jclass cls) { return 55; }
JNIEXPORT jint JNICALL Java_L1W63JvmtiEvents_boundLater(JNIEnv *env, jclass cls, jint x);
JNIEXPORT jint JNICALL Java_L1W63JvmtiEvents_replaced(JNIEnv *env, jclass cls);

static void JNICALL on_native_bind(jvmtiEnv *jvmti, JNIEnv *jni, jthread t, jmethodID m, void *address, void **new_address) {
    jclass klass = NULL;
    char sig[256], name[128];
    void *own = NULL;
    (*jvmti)->GetMethodDeclaringClass(jvmti, m, &klass);
    signature_of(jvmti, klass, sig, sizeof sig);
    if (strcmp(sig, "LL1W63JvmtiEvents;") != 0) return;
    method_name(jvmti, m, name, sizeof name);
    if (strcmp(name, "boundLater") == 0) own = (void *)Java_L1W63JvmtiEvents_boundLater;
    if (strcmp(name, "registered") == 0) own = (void *)registered_impl;
    if (strcmp(name, "replaced") == 0) own = (void *)Java_L1W63JvmtiEvents_replaced;
    if (strcmp(name, "replaced") == 0 && new_address != NULL) *new_address = (void *)replacement;
    record("bind %s address-is-own=%d", name, address == own);
}

static void JNICALL on_compiled_load(jvmtiEnv *jvmti, jmethodID m, jint size, const void *addr, jint n,
                                     const jvmtiAddrLocationMap *map, const void *info) {
    char name[128];
    generated++;
    method_name(jvmti, m, name, sizeof name);
    if (strcmp(name, "hot") == 0 && size > 0) record("load hot");
}

static void JNICALL on_dynamic_code(jvmtiEnv *jvmti, const char *name, const void *addr, jint length) {
    if (name != NULL && length > 0) generated++;
}

static void monitor_line(jvmtiEnv *jvmti, const char *kind, jthread t, jobject o, const char *extra) {
    char tn[128];
    if (tag_of(jvmti, o) != 1) return;
    thread_name(jvmti, t, tn, sizeof tn);
    record("monitor %s by %s%s", kind, tn, extra);
}

static void JNICALL on_monitor_wait(jvmtiEnv *jvmti, JNIEnv *jni, jthread t, jobject o, jlong timeout) {
    char extra[64];
    snprintf(extra, sizeof extra, " timeout=%lld", (long long)timeout);
    monitor_line(jvmti, "wait", t, o, extra);
}

static void JNICALL on_monitor_waited(jvmtiEnv *jvmti, JNIEnv *jni, jthread t, jobject o, jboolean timed_out) {
    char extra[64];
    snprintf(extra, sizeof extra, " timed_out=%d", timed_out);
    monitor_line(jvmti, "waited", t, o, extra);
}

static void JNICALL on_contended_enter(jvmtiEnv *jvmti, JNIEnv *jni, jthread t, jobject o) {
    monitor_line(jvmti, "contended-enter", t, o, "");
}

static void JNICALL on_contended_entered(jvmtiEnv *jvmti, JNIEnv *jni, jthread t, jobject o) {
    monitor_line(jvmti, "contended-entered", t, o, "");
}

static void JNICALL on_exhausted(jvmtiEnv *jvmti, JNIEnv *jni, jint flags, const void *reserved, const char *description) {
    record("exhausted flags=%d description=%s", flags, description == NULL ? "null" : description);
}

static void JNICALL on_gc_start(jvmtiEnv *jvmti) { gc_starts++; }
static void JNICALL on_gc_finish(jvmtiEnv *jvmti) { gc_finishes++; }

static void JNICALL on_object_free(jvmtiEnv *jvmti, jlong tag) { record("free tag=%lld", (long long)tag); }

static void JNICALL on_vm_object_alloc(jvmtiEnv *jvmti, JNIEnv *jni, jthread t, jobject o, jclass klass, jlong size) {
    char sig[256];
    signature_of(jvmti, klass, sig, sizeof sig);
    if (strstr(sig, "Made") != NULL || strcmp(sig, "[I") == 0 || strcmp(sig, "[[I") == 0) {
        record("vm-alloc %s size=%lld", sig, (long long)size);
    }
}

static void JNICALL on_sampled(jvmtiEnv *jvmti, JNIEnv *jni, jthread t, jobject o, jclass klass, jlong size) {
    char sig[256];
    signature_of(jvmti, klass, sig, sizeof sig);
    if (strstr(sig, "Sampled") != NULL) sampled++;
}

static void vt_line(jvmtiEnv *jvmti, const char *kind, jthread vt) {
    char tn[128];
    thread_name(jvmti, vt, tn, sizeof tn);
    if (strcmp(tn, "l1w63-vt") == 0) record("vt %s", kind);
}

static void JNICALL on_vt_start(jvmtiEnv *jvmti, JNIEnv *jni, jthread vt) { vt_line(jvmti, "start", vt); }
static void JNICALL on_vt_end(jvmtiEnv *jvmti, JNIEnv *jni, jthread vt) { vt_line(jvmti, "end", vt); }

static void JNICALL on_vt_mount(jvmtiEnv *jvmti, ...) {
    va_list ap;
    jthread vt;
    va_start(ap, jvmti);
    (void)va_arg(ap, JNIEnv *);
    vt = va_arg(ap, jthread);
    va_end(ap);
    vt_line(jvmti, "mount", vt);
}

static void JNICALL on_vt_unmount(jvmtiEnv *jvmti, ...) {
    va_list ap;
    jthread vt;
    va_start(ap, jvmti);
    (void)va_arg(ap, JNIEnv *);
    vt = va_arg(ap, jthread);
    va_end(ap);
    vt_line(jvmti, "unmount", vt);
}

static void JNICALL on_class_unload(jvmtiEnv *jvmti, ...) {
    va_list ap;
    const char *name;
    va_start(ap, jvmti);
    (void)va_arg(ap, JNIEnv *);
    name = va_arg(ap, const char *);
    va_end(ap);
    if (name != NULL && strstr(name, "L1W63") != NULL) record("unload %s", name);
}

static void on_thread(jvmtiEnv *jvmti, jthread t, int start) {
    char tn[128];
    thread_name(jvmti, t, tn, sizeof tn);
    if (strncmp(tn, "l1w63-", 6) == 0) record("thread %s %s", start ? "start" : "end", tn);
}

static void JNICALL on_thread_start(jvmtiEnv *jvmti, JNIEnv *jni, jthread t) { on_thread(jvmti, t, 1); }
static void JNICALL on_thread_end(jvmtiEnv *jvmti, JNIEnv *jni, jthread t) { on_thread(jvmti, t, 0); }

JNIEXPORT jint JNICALL Agent_OnLoad(JavaVM *vm, char *options, void *reserved) {
    jvmtiCapabilities caps;
    jvmtiEventCallbacks callbacks;
    if ((*vm)->GetEnv(vm, (void **)&agent, JVMTI_VERSION_1_2) != JNI_OK) return 1;
    memset(&caps, 0, sizeof caps);
    (*agent)->GetPotentialCapabilities(agent, &caps);
    (*agent)->AddCapabilities(agent, &caps);
    (*agent)->CreateRawMonitor(agent, "l1w63-lines", &lines_lock);
    memset(&callbacks, 0, sizeof callbacks);
    callbacks.ThreadStart = on_thread_start;
    callbacks.ThreadEnd = on_thread_end;
    callbacks.FieldAccess = on_field_access;
    callbacks.FieldModification = on_field_modification;
    callbacks.NativeMethodBind = on_native_bind;
    callbacks.CompiledMethodLoad = on_compiled_load;
    callbacks.DynamicCodeGenerated = on_dynamic_code;
    callbacks.MonitorWait = on_monitor_wait;
    callbacks.MonitorWaited = on_monitor_waited;
    callbacks.MonitorContendedEnter = on_contended_enter;
    callbacks.MonitorContendedEntered = on_contended_entered;
    callbacks.ResourceExhausted = on_exhausted;
    callbacks.GarbageCollectionStart = on_gc_start;
    callbacks.GarbageCollectionFinish = on_gc_finish;
    callbacks.ObjectFree = on_object_free;
    callbacks.VMObjectAlloc = on_vm_object_alloc;
    callbacks.SampledObjectAlloc = on_sampled;
    callbacks.VirtualThreadStart = on_vt_start;
    callbacks.VirtualThreadEnd = on_vt_end;
    (*agent)->SetEventCallbacks(agent, &callbacks, sizeof callbacks);
    return 0;
}

#define NEED_AGENT(env) if (agent == NULL) return (*env)->NewStringUTF(env, NO_ENV)

JNIEXPORT jint JNICALL Java_L1W63JvmtiEvents_enable(JNIEnv *env, jclass cls, jint event, jboolean on) {
    if (agent == NULL) return -1;
    return (*agent)->SetEventNotificationMode(agent, on ? JVMTI_ENABLE : JVMTI_DISABLE, (jvmtiEvent)event, NULL);
}

JNIEXPORT jstring JNICALL Java_L1W63JvmtiEvents_drain(JNIEnv *env, jclass cls) {
    static char out[65536];
    int i, k, starts, finishes, n;
    NEED_AGENT(env);
    out[0] = 0;
    starts = gc_starts;
    finishes = gc_finishes;
    gc_starts = gc_finishes = 0;
    if (starts != 0 || finishes != 0) {
        append(out, sizeof out, "gc started=%d finished=%d balanced=%d\n", starts > 0, finishes > 0, starts == finishes);
    }
    n = sampled;
    sampled = 0;
    if (n != 0) append(out, sizeof out, "sampled Sampled: posted\n");
    (*agent)->RawMonitorEnter(agent, lines_lock);
    for (i = 0; i < nlines; i++) {
        int dup = 0;
        if (strcmp(lines[i], "load hot") == 0) {
            for (k = 0; k < i; k++) dup |= lines[k] != NULL && strcmp(lines[k], "load hot") == 0;
        }
        if (!dup) append(out, sizeof out, "%s\n", lines[i]);
    }
    for (i = 0; i < nlines; i++) free(lines[i]);
    nlines = 0;
    (*agent)->RawMonitorExit(agent, lines_lock);
    return (*env)->NewStringUTF(env, out);
}

JNIEXPORT jint JNICALL Java_L1W63JvmtiEvents_watch(JNIEnv *env, jclass cls, jclass klass, jstring name, jstring sig,
                                                   jboolean is_static, jboolean access, jboolean set) {
    const char *n = (*env)->GetStringUTFChars(env, name, NULL);
    const char *s = (*env)->GetStringUTFChars(env, sig, NULL);
    jfieldID field = is_static ? (*env)->GetStaticFieldID(env, klass, n, s) : (*env)->GetFieldID(env, klass, n, s);
    jvmtiError e;
    (*env)->ReleaseStringUTFChars(env, name, n);
    (*env)->ReleaseStringUTFChars(env, sig, s);
    if (agent == NULL) return -1;
    if (access) {
        e = set ? (*agent)->SetFieldAccessWatch(agent, klass, field) : (*agent)->ClearFieldAccessWatch(agent, klass, field);
    } else {
        e = set ? (*agent)->SetFieldModificationWatch(agent, klass, field)
                : (*agent)->ClearFieldModificationWatch(agent, klass, field);
    }
    return e;
}

JNIEXPORT jstring JNICALL Java_L1W63JvmtiEvents_generate(JNIEnv *env, jclass cls, jint event) {
    char out[128];
    jvmtiError e;
    int n;
    NEED_AGENT(env);
    (*agent)->SetEventNotificationMode(agent, JVMTI_ENABLE, (jvmtiEvent)event, NULL);
    generated = 0;
    e = (*agent)->GenerateEvents(agent, (jvmtiEvent)event);
    (*agent)->SetEventNotificationMode(agent, JVMTI_DISABLE, (jvmtiEvent)event, NULL);
    n = generated;
    generated = 0;
    snprintf(out, sizeof out, "err=%d posted=%d", e, n > 0);
    return (*env)->NewStringUTF(env, out);
}

JNIEXPORT jint JNICALL Java_L1W63JvmtiEvents_sampling(JNIEnv *env, jclass cls, jint n) {
    return (*agent)->SetHeapSamplingInterval(agent, n);
}

JNIEXPORT jint JNICALL Java_L1W63JvmtiEvents_extension(JNIEnv *env, jclass cls, jint index, jboolean on) {
    jvmtiExtensionEvent f = NULL;
    if (on) f = index == 49 ? on_class_unload : index == 48 ? on_vt_mount : on_vt_unmount;
    return (*agent)->SetExtensionEventCallback(agent, index, f);
}

static void JNICALL agent_body(jvmtiEnv *jvmti, JNIEnv *jni, void *arg) {}

JNIEXPORT jint JNICALL Java_L1W63JvmtiEvents_runAgent(JNIEnv *env, jclass cls, jthread t) {
    return (*agent)->RunAgentThread(agent, t, agent_body, NULL, 5);
}

JNIEXPORT jobject JNICALL Java_L1W63JvmtiEvents_jniAlloc(JNIEnv *env, jclass cls, jclass klass, jboolean construct) {
    jmethodID init;
    if (!construct) return (*env)->AllocObject(env, klass);
    init = (*env)->GetMethodID(env, klass, "<init>", "()V");
    return (*env)->NewObjectA(env, klass, init, NULL);
}

JNIEXPORT jint JNICALL Java_L1W63JvmtiEvents_boundLater(JNIEnv *env, jclass cls, jint x) { return x + 1; }

JNIEXPORT jint JNICALL Java_L1W63JvmtiEvents_registerNow(JNIEnv *env, jclass cls) {
    JNINativeMethod methods[1] = {{"registered", "()I", (void *)registered_impl}};
    return (*env)->RegisterNatives(env, cls, methods, 1);
}

JNIEXPORT jint JNICALL Java_L1W63JvmtiEvents_replaced(JNIEnv *env, jclass cls) { return 1; }

JNIEXPORT jint JNICALL Java_L1W63JvmtiEvents_tag(JNIEnv *env, jclass cls, jobject o, jlong tag) {
    return (*agent)->SetTag(agent, o, tag);
}
