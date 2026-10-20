/*
 * Native shim and agent for tools/probes/interp/L1/L1W60JvmtiLambdaFrames.java
 * (interpreter round i2 wave 60, lane L1). Loaded twice: as an agent
 * (`-agentpath:...`), whose `Agent_OnLoad` keeps the JVMTI env, and with
 * `System.load`, for the native `stack`. Build instructions are in the
 * probe's header; the Rust port
 * tools/probes/interp/L1/rust-ports/L1W60JvmtiLambdaFrames prints the same lines.
 */
#include <jni.h>
#include <jvmti.h>
#include <stdarg.h>
#include <stdio.h>
#include <string.h>

static char out[8192];
static jvmtiEnv *agent_env = NULL;

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

/* `text` with every hex digit after "0x" as 'h' (the suffix of a hidden
 * class's name is an address on HotSpot, an id on CratonVM). */
static void mask(const char *text, char *masked, size_t size)
{
    char *hex;
    size_t i;
    snprintf(masked, size, "%s", text);
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

JNIEXPORT jint JNICALL
Agent_OnLoad(JavaVM *vm, char *options, void *reserved)
{
    jvmtiEnv *jvmti = NULL;
    (void)options;
    (void)reserved;
    if ((*vm)->GetEnv(vm, (void **)&jvmti, JVMTI_VERSION_1_2) != JNI_OK) {
        return 1;
    }
    out[0] = '\0';
    agent_env = jvmti;
    return 0;
}

/* `Class.method(signature)` of `m`, the class's signature masked. */
static void name_of(jvmtiEnv *jvmti, jmethodID m, char *named, size_t size)
{
    jclass declaring = NULL;
    char *csig = NULL;
    char *name = NULL;
    char *sig = NULL;
    char masked[512];
    (*jvmti)->GetMethodDeclaringClass(jvmti, m, &declaring);
    (*jvmti)->GetClassSignature(jvmti, declaring, &csig, NULL);
    (*jvmti)->GetMethodName(jvmti, m, &name, &sig, NULL);
    mask(csig == NULL ? "?" : csig, masked, sizeof masked);
    snprintf(named, size, "%s %s%s", masked, name == NULL ? "?" : name, sig == NULL ? "" : sig);
    (*jvmti)->Deallocate(jvmti, (unsigned char *)csig);
    (*jvmti)->Deallocate(jvmti, (unsigned char *)name);
    (*jvmti)->Deallocate(jvmti, (unsigned char *)sig);
}

/* Frames 1..5 under this native (main is the outermost frame of the
 * probe's thread, so every frame), then the frame count and the frame at
 * depth 2. */
JNIEXPORT jstring JNICALL
Java_L1W60JvmtiLambdaFrames_stack(JNIEnv *env, jclass self)
{
    jvmtiEnv *jvmti = agent_env;
    jvmtiFrameInfo frames[6];
    jint count = 0;
    jint depth = 0;
    jmethodID at = NULL;
    jlocation location = 0;
    char named[1024];
    jstring answer;
    int err;
    int cerr;
    int lerr;
    jint i;
    (void)self;
    if (jvmti == NULL) {
        return (*env)->NewStringUTF(env, "no JVMTI env (load the shim with -agentpath too)\n");
    }
    err = (int)(*jvmti)->GetStackTrace(jvmti, NULL, 0, 6, frames, &count);
    line("  stack: err=%d count=%d", err, (int)count);
    for (i = 1; err == 0 && i < count; i++) {
        name_of(jvmti, frames[i].method, named, sizeof named);
        line("    frame %d: %s at %lld", (int)i, named, (long long)frames[i].location);
    }
    cerr = (int)(*jvmti)->GetFrameCount(jvmti, NULL, &depth);
    lerr = (int)(*jvmti)->GetFrameLocation(jvmti, NULL, 2, &at, &location);
    if (lerr == 0) {
        name_of(jvmti, at, named, sizeof named);
    } else {
        snprintf(named, sizeof named, "-");
    }
    line("  frame count: err=%d count=%d", cerr, (int)depth);
    line("  depth 2: err=%d %s at %lld", lerr, named, (long long)(lerr == 0 ? location : -1));
    answer = (*env)->NewStringUTF(env, out);
    out[0] = '\0';
    return answer;
}

/* `GetMethodID(klass, name, sig)`: found, and the declaring class (its
 * signature masked, whether it is `klass`) and modifiers of the answer. */
static jmethodID report(JNIEnv *env, jvmtiEnv *jvmti, jclass klass, const char *name, const char *sig)
{
    jmethodID m = (*env)->GetMethodID(env, klass, name, sig);
    jclass declaring = NULL;
    char *csig = NULL;
    char masked[512];
    jint modifiers = 0;
    int derr;
    int merr;
    if (m == NULL) {
        int pending = (int)(*env)->ExceptionCheck(env);
        (*env)->ExceptionClear(env);
        line("  %s%s: found=0 pending=%d", name, sig, pending);
        return NULL;
    }
    derr = (int)(*jvmti)->GetMethodDeclaringClass(jvmti, m, &declaring);
    if (derr == 0) {
        (*jvmti)->GetClassSignature(jvmti, declaring, &csig, NULL);
    }
    mask(csig == NULL ? "?" : csig, masked, sizeof masked);
    merr = (int)(*jvmti)->GetMethodModifiers(jvmti, m, &modifiers);
    line("  %s%s: found=1 declaring err=%d %s own=%d modifiers err=%d 0x%x", name, sig, derr, masked,
         derr == 0 ? (int)(*env)->IsSameObject(env, declaring, klass) : -1, merr, (unsigned)modifiers);
    (*jvmti)->Deallocate(jvmti, (unsigned char *)csig);
    return m;
}

/* JNI's view of a lambda's class: `GetMethodID` of the methods it
 * declares and of one it inherits, then a call through its own `run`. */
JNIEXPORT jstring JNICALL
Java_L1W60JvmtiLambdaFrames_ids(JNIEnv *env, jclass self, jobject lambda)
{
    jvmtiEnv *jvmti = agent_env;
    jclass klass;
    jmethodID run;
    jstring answer;
    (void)self;
    if (jvmti == NULL) {
        return (*env)->NewStringUTF(env, "no JVMTI env (load the shim with -agentpath too)\n");
    }
    klass = (*env)->GetObjectClass(env, lambda);
    run = report(env, jvmti, klass, "run", "()V");
    report(env, jvmti, klass, "<init>", "()V");
    report(env, jvmti, klass, "toString", "()Ljava/lang/String;");
    if (run != NULL) {
        (*env)->CallVoidMethodA(env, lambda, run, NULL);
        line("  call: pending=%d", (int)(*env)->ExceptionCheck(env));
        (*env)->ExceptionClear(env);
    }
    answer = (*env)->NewStringUTF(env, out);
    out[0] = '\0';
    return answer;
}
