/*
 * Native shim and agent for tools/probes/interp/L1/L1W54JvmtiRetransform.java
 * (interpreter round i2 wave 54, lane L1). Loaded twice: as an agent
 * (`-agentpath:...`), whose `Agent_OnLoad` adds `can_retransform_classes` and
 * enables a `ClassFileLoadHook`, and with `System.load`, for the natives.
 * Build instructions are in the probe's header; the Rust port
 * tools/probes/interp/L1/rust-ports/L1W54JvmtiRetransform prints the same lines.
 */
#include <jni.h>
#include <jvmti.h>
#include <stdarg.h>
#include <stdio.h>
#include <string.h>

#define PREFIX "L1W54JvmtiRetransform$"

static char out[16384];
static jvmtiEnv *agent_env = NULL;
static JavaVM *java_vm = NULL;
static int live = 0;
/* Start-phase offers with a NULL / a non-NULL loader, and offers in a
 * phase before the start phase (none without
 * can_generate_early_class_hook_events). */
static long start_boot = 0;
static long start_other = 0;
static long early_phase = 0;

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

static int count_of(const unsigned char *data, jint len, const char *word)
{
    int n = 0;
    jint i;
    for (i = 0; i + 6 <= len; i++) {
        if (memcmp(data + i, word, 6) == 0) {
            n++;
        }
    }
    return n;
}

static void JNICALL
on_class_file_load_hook(jvmtiEnv *jvmti, JNIEnv *env, jclass class_being_redefined,
                        jobject loader, const char *name, jobject protection_domain,
                        jint class_data_len, const unsigned char *class_data,
                        jint *new_class_data_len, unsigned char **new_class_data)
{
    jvmtiPhase phase = (jvmtiPhase)0;
    int rewritten = -1;
    if (!live) {
        (*jvmti)->GetPhase(jvmti, &phase);
        if (phase == JVMTI_PHASE_START) {
            if (loader == NULL) {
                start_boot++;
            } else {
                start_other++;
            }
        } else if (phase != JVMTI_PHASE_LIVE) {
            early_phase++;
        }
        return;
    }
    if (name == NULL || strncmp(name, PREFIX, strlen(PREFIX)) != 0) {
        return;
    }
    if (class_being_redefined != NULL) {
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
    line("hook %s: redefined=%s loader=%s domain=%s before=%d after=%d rewritten=%d", name,
         class_being_redefined == NULL ? "null" : "class", loader_kind(env, loader),
         protection_domain == NULL ? "null" : "set", count_of(class_data, class_data_len, "BEFORE"),
         count_of(class_data, class_data_len, "AFTER!"), rewritten);
}

static void JNICALL
on_vm_init(jvmtiEnv *jvmti, JNIEnv *env, jthread thread)
{
    (void)jvmti;
    (void)env;
    (void)thread;
    live = 1;
    line("start phase: boot=%s other=%s earlier=%s", start_boot > 0 ? "yes" : "no",
         start_other > 0 ? "yes" : "no", early_phase > 0 ? "yes" : "no");
}

/* Whether `caps` has can_retransform_classes. (HotSpot 25.0.3 also lists
 * can_redefine_classes and can_generate_all_class_hook_events in both phases
 * and can_generate_early_class_hook_events in OnLoad; the C table grants
 * none of them yet.) */
static void caps_text(char *text, size_t size, const jvmtiCapabilities *caps)
{
    snprintf(text, size, "retransform=%d", (int)caps->can_retransform_classes);
}

JNIEXPORT jint JNICALL
Agent_OnLoad(JavaVM *vm, char *options, void *reserved)
{
    jvmtiEnv *jvmti = NULL;
    jvmtiEventCallbacks callbacks;
    jvmtiCapabilities caps;
    char text[160];
    int err;
    (void)options;
    (void)reserved;
    if ((*vm)->GetEnv(vm, (void **)&jvmti, JVMTI_VERSION_1_2) != JNI_OK) {
        return 1;
    }
    out[0] = '\0';
    memset(&caps, 0, sizeof caps);
    err = (int)(*jvmti)->GetPotentialCapabilities(jvmti, &caps);
    caps_text(text, sizeof text, &caps);
    line("onload potential: err=%d %s", err, text);
    memset(&caps, 0, sizeof caps);
    caps.can_retransform_classes = 1;
    line("add can_retransform_classes: %d", (int)(*jvmti)->AddCapabilities(jvmti, &caps));
    memset(&callbacks, 0, sizeof callbacks);
    callbacks.ClassFileLoadHook = &on_class_file_load_hook;
    callbacks.VMInit = &on_vm_init;
    err = (int)(*jvmti)->SetEventCallbacks(jvmti, &callbacks, (jint)sizeof callbacks);
    line("onload: callbacks=%d cflh=%d init=%d", err,
         (int)(*jvmti)->SetEventNotificationMode(jvmti, JVMTI_ENABLE,
                                                 JVMTI_EVENT_CLASS_FILE_LOAD_HOOK, NULL),
         (int)(*jvmti)->SetEventNotificationMode(jvmti, JVMTI_ENABLE, JVMTI_EVENT_VM_INIT,
                                                 NULL));
    agent_env = jvmti;
    java_vm = vm;
    return 0;
}

#define NO_ENV "no JVMTI env (load the shim with -agentpath too)"

/* RetransformClasses(count, the elements of classes). */
JNIEXPORT jstring JNICALL
Java_L1W54JvmtiRetransform_retransform(JNIEnv *env, jclass self, jobjectArray classes, jint count)
{
    jclass list[8];
    jint length;
    jint i;
    char text[64];
    (void)self;
    if (agent_env == NULL) {
        return (*env)->NewStringUTF(env, NO_ENV);
    }
    length = (*env)->GetArrayLength(env, classes);
    for (i = 0; i < length && i < 8; i++) {
        list[i] = (jclass)(*env)->GetObjectArrayElement(env, classes, i);
    }
    snprintf(text, sizeof text, "err=%d",
             (int)(*agent_env)->RetransformClasses(agent_env, count, list));
    return (*env)->NewStringUTF(env, text);
}

JNIEXPORT jstring JNICALL
Java_L1W54JvmtiRetransform_retransformNull(JNIEnv *env, jclass self)
{
    char text[64];
    (void)self;
    if (agent_env == NULL) {
        return (*env)->NewStringUTF(env, NO_ENV);
    }
    snprintf(text, sizeof text, "err=%d",
             (int)(*agent_env)->RetransformClasses(agent_env, 1, NULL));
    return (*env)->NewStringUTF(env, text);
}

/* RetransformClasses through a second env, which has no capability. */
JNIEXPORT jstring JNICALL
Java_L1W54JvmtiRetransform_withoutCapability(JNIEnv *env, jclass self, jclass klass)
{
    jvmtiEnv *other = NULL;
    char text[64];
    (void)self;
    if (java_vm == NULL || (*java_vm)->GetEnv(java_vm, (void **)&other, JVMTI_VERSION_1_2)
                               != JNI_OK) {
        return (*env)->NewStringUTF(env, "no second env");
    }
    snprintf(text, sizeof text, "err=%d", (int)(*other)->RetransformClasses(other, 1, &klass));
    (*other)->DisposeEnvironment(other);
    return (*env)->NewStringUTF(env, text);
}

JNIEXPORT jstring JNICALL
Java_L1W54JvmtiRetransform_livePotential(JNIEnv *env, jclass self)
{
    jvmtiCapabilities caps;
    char text[160];
    char line_text[200];
    int err;
    (void)self;
    if (agent_env == NULL) {
        return (*env)->NewStringUTF(env, NO_ENV);
    }
    memset(&caps, 0, sizeof caps);
    err = (int)(*agent_env)->GetPotentialCapabilities(agent_env, &caps);
    caps_text(text, sizeof text, &caps);
    snprintf(line_text, sizeof line_text, "err=%d %s", err, text);
    return (*env)->NewStringUTF(env, line_text);
}

/* Which of the four classes in queries GetClassLoaderClasses lists for loader. */
JNIEXPORT jstring JNICALL
Java_L1W54JvmtiRetransform_loaderClasses(JNIEnv *env, jclass self, jobject loader,
                                         jobjectArray queries)
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
        return (*env)->NewStringUTF(env, NO_ENV);
    }
    err = (int)(*agent_env)->GetClassLoaderClasses(agent_env, loader, &count, &classes);
    for (q = 0; q < 4 && err == 0; q++) {
        jobject query = (*env)->GetObjectArrayElement(env, queries, q);
        for (i = 0; i < count; i++) {
            if ((*env)->IsSameObject(env, classes[i], query)) {
                found[q] = 1;
            }
        }
    }
    (*agent_env)->Deallocate(agent_env, (unsigned char *)classes);
    snprintf(text, sizeof text, "err=%d probe=%d target=%d target[]=%d string=%d", err,
             found[0], found[1], found[2], found[3]);
    return (*env)->NewStringUTF(env, text);
}

/* IsModifiableClass of the five classes in queries: the answer, or -err. */
JNIEXPORT jstring JNICALL
Java_L1W54JvmtiRetransform_modifiable(JNIEnv *env, jclass self, jobjectArray queries)
{
    int answers[5];
    char text[128];
    int q;
    (void)self;
    if (agent_env == NULL) {
        return (*env)->NewStringUTF(env, NO_ENV);
    }
    for (q = 0; q < 5; q++) {
        jboolean modifiable = JNI_FALSE;
        jclass klass = (jclass)(*env)->GetObjectArrayElement(env, queries, q);
        int err = (int)(*agent_env)->IsModifiableClass(agent_env, klass, &modifiable);
        answers[q] = err != 0 ? -err : (int)modifiable;
    }
    snprintf(text, sizeof text, "target=%d int=%d int[]=%d string=%d lambda=%d", answers[0],
             answers[1], answers[2], answers[3], answers[4]);
    return (*env)->NewStringUTF(env, text);
}

/* Whether GetLoadedClasses lists a lambda's class, and GetClassSignature's
 * error for it. */
JNIEXPORT jstring JNICALL
Java_L1W54JvmtiRetransform_lambdaFacts(JNIEnv *env, jclass self, jclass lambda)
{
    jint count = 0;
    jclass *classes = NULL;
    char *sig = NULL;
    int loaded = 0;
    int err;
    int sig_err;
    char text[96];
    jint i;
    (void)self;
    if (agent_env == NULL) {
        return (*env)->NewStringUTF(env, NO_ENV);
    }
    err = (int)(*agent_env)->GetLoadedClasses(agent_env, &count, &classes);
    for (i = 0; err == 0 && i < count; i++) {
        if ((*env)->IsSameObject(env, classes[i], lambda)) {
            loaded = 1;
        }
    }
    (*agent_env)->Deallocate(agent_env, (unsigned char *)classes);
    sig_err = (int)(*agent_env)->GetClassSignature(agent_env, lambda, &sig, NULL);
    if (sig != NULL) {
        (*agent_env)->Deallocate(agent_env, (unsigned char *)sig);
    }
    snprintf(text, sizeof text, "loaded=%d signature-err=%d", loaded, sig_err);
    return (*env)->NewStringUTF(env, text);
}

JNIEXPORT jstring JNICALL
Java_L1W54JvmtiRetransform_rows(JNIEnv *env, jclass self)
{
    jstring answer;
    (void)self;
    if (agent_env == NULL) {
        return (*env)->NewStringUTF(env, NO_ENV "\n");
    }
    answer = (*env)->NewStringUTF(env, out);
    out[0] = '\0';
    return answer;
}
