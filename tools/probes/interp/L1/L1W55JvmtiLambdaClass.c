/*
 * Native shim and agent for tools/probes/interp/L1/L1W55JvmtiLambdaClass.java
 * (interpreter round i2 wave 55, lane L1). Loaded twice: as an agent
 * (`-agentpath:...`), whose `Agent_OnLoad` adds `can_get_source_file_name`,
 * and with `System.load`, for the native `facts`. Build instructions are in
 * the probe's header; the Rust port
 * tools/probes/interp/L1/rust-ports/L1W55JvmtiLambdaClass prints the same lines.
 */
#include <jni.h>
#include <jvmti.h>
#include <stdarg.h>
#include <stdio.h>
#include <string.h>

static char out[4096];
static jvmtiEnv *agent_env = NULL;

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

/* The signature with every hex digit after "0x" as 'h' (the address
 * differs between runs and VMs). */
static void mask(const char *sig, char *masked, size_t size)
{
    const char *hex;
    size_t i;
    snprintf(masked, size, "%s", sig);
    hex = strstr(masked, "0x");
    if (hex == NULL) {
        return;
    }
    for (i = (size_t)(hex - masked) + 2; masked[i] != '\0'; i++) {
        char c = masked[i];
        if ((c >= '0' && c <= '9') || (c >= 'a' && c <= 'f') || (c >= 'A' && c <= 'F')) {
            masked[i] = 'h';
        } else {
            break;
        }
    }
}

static const char *loader_kind(JNIEnv *env, jobject loader, jobject app)
{
    if (loader == NULL) {
        return "null";
    }
    return (*env)->IsSameObject(env, loader, app) ? "app" : "other";
}

static int listed(JNIEnv *env, jint count, jclass *classes, jclass klass)
{
    jint i;
    for (i = 0; i < count; i++) {
        if ((*env)->IsSameObject(env, classes[i], klass)) {
            return 1;
        }
    }
    return 0;
}

JNIEXPORT jint JNICALL
Agent_OnLoad(JavaVM *vm, char *options, void *reserved)
{
    jvmtiEnv *jvmti = NULL;
    jvmtiCapabilities wanted;
    (void)options;
    (void)reserved;
    if ((*vm)->GetEnv(vm, (void **)&jvmti, JVMTI_VERSION_1_2) != JNI_OK) {
        return 1;
    }
    memset(&wanted, 0, sizeof wanted);
    wanted.can_get_source_file_name = 1;
    out[0] = '\0';
    line("onload: add=%d", (int)(*jvmti)->AddCapabilities(jvmti, &wanted));
    agent_env = jvmti;
    return 0;
}

JNIEXPORT jstring JNICALL
Java_L1W55JvmtiLambdaClass_facts(JNIEnv *env, jclass self, jclass klass, jobject app)
{
    jvmtiEnv *jvmti = agent_env;
    jint count = 0;
    jclass *classes = NULL;
    char *sig = NULL;
    char *generic = NULL;
    char masked[256];
    jint modifiers = 0;
    jobject loader = NULL;
    jint status = 0;
    jboolean is_interface = 0;
    jboolean is_array = 0;
    jmethodID *methods = NULL;
    char *source = NULL;
    jint minor = 0;
    jint major = 0;
    jstring answer;
    int err;
    (void)self;
    if (jvmti == NULL) {
        return (*env)->NewStringUTF(env, "no JVMTI env (load the shim with -agentpath too)\n");
    }
    err = (int)(*jvmti)->GetLoadedClasses(jvmti, &count, &classes);
    line("loaded classes: err=%d listed=%d", err, err == 0 ? listed(env, count, classes, klass) : 0);
    (*jvmti)->Deallocate(jvmti, (unsigned char *)classes);
    classes = NULL;
    err = (int)(*jvmti)->GetClassSignature(jvmti, klass, &sig, &generic);
    mask(err == 0 ? sig : "-", masked, sizeof masked);
    line("signature: err=%d sig=%s generic=%s", err, masked, generic == NULL ? "null" : "set");
    (*jvmti)->Deallocate(jvmti, (unsigned char *)sig);
    (*jvmti)->Deallocate(jvmti, (unsigned char *)generic);
    err = (int)(*jvmti)->GetClassModifiers(jvmti, klass, &modifiers);
    line("modifiers: err=%d value=0x%x", err, (unsigned)modifiers);
    err = (int)(*jvmti)->GetClassLoader(jvmti, klass, &loader);
    line("loader: err=%d kind=%s", err, err == 0 ? loader_kind(env, loader, app) : "-");
    err = (int)(*jvmti)->GetImplementedInterfaces(jvmti, klass, &count, &classes);
    sig = NULL;
    if (err == 0 && count > 0) {
        (*jvmti)->GetClassSignature(jvmti, classes[0], &sig, NULL);
    }
    line("interfaces: err=%d count=%d first=%s", err, err == 0 ? (int)count : -1,
         sig == NULL ? "-" : sig);
    (*jvmti)->Deallocate(jvmti, (unsigned char *)sig);
    (*jvmti)->Deallocate(jvmti, (unsigned char *)classes);
    classes = NULL;
    err = (int)(*jvmti)->GetClassStatus(jvmti, klass, &status);
    line("status: err=%d value=%d", err, (int)status);
    line("kind: interface err=%d value=%d array err=%d value=%d",
         (int)(*jvmti)->IsInterface(jvmti, klass, &is_interface), (int)is_interface,
         (int)(*jvmti)->IsArrayClass(jvmti, klass, &is_array), (int)is_array);
    count = 0;
    err = (int)(*jvmti)->GetClassMethods(jvmti, klass, &count, &methods);
    line("methods: err=%d count=%d", err, (int)count);
    (*jvmti)->Deallocate(jvmti, (unsigned char *)methods);
    err = (int)(*jvmti)->GetSourceFileName(jvmti, klass, &source);
    line("source file: err=%d", err);
    (*jvmti)->Deallocate(jvmti, (unsigned char *)source);
    err = (int)(*jvmti)->GetClassVersionNumbers(jvmti, klass, &minor, &major);
    line("version: err=%d major=%d", err, (int)major);
    count = 0;
    err = (int)(*jvmti)->GetClassLoaderClasses(jvmti, app, &count, &classes);
    line("app loader classes: err=%d listed=%d", err,
         err == 0 ? listed(env, count, classes, klass) : 0);
    (*jvmti)->Deallocate(jvmti, (unsigned char *)classes);
    answer = (*env)->NewStringUTF(env, out);
    out[0] = '\0';
    return answer;
}
