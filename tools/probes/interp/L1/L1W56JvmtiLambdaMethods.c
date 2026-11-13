/*
 * Native shim and agent for tools/probes/interp/L1/L1W56JvmtiLambdaMethods.java
 * (interpreter round i2 wave 56, lane L1). Loaded twice: as an agent
 * (`-agentpath:...`), whose `Agent_OnLoad` adds `can_get_bytecodes`,
 * `can_get_synthetic_attribute` and `can_get_line_numbers`, and with
 * `System.load`, for the natives `methods` and `stack`. Build instructions
 * are in the probe's header; the Rust port
 * tools/probes/interp/L1/rust-ports/L1W56JvmtiLambdaMethods prints the same lines.
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
    jvmtiCapabilities wanted;
    (void)options;
    (void)reserved;
    if ((*vm)->GetEnv(vm, (void **)&jvmti, JVMTI_VERSION_1_2) != JNI_OK) {
        return 1;
    }
    memset(&wanted, 0, sizeof wanted);
    wanted.can_get_bytecodes = 1;
    wanted.can_get_synthetic_attribute = 1;
    wanted.can_get_line_numbers = 1;
    out[0] = '\0';
    line("onload: add=%d", (int)(*jvmti)->AddCapabilities(jvmti, &wanted));
    agent_env = jvmti;
    return 0;
}

static void method_row(JNIEnv *env, jvmtiEnv *jvmti, jclass klass, int i, jmethodID m)
{
    char *name = NULL;
    char *sig = NULL;
    char *generic = NULL;
    jclass declaring = NULL;
    jint modifiers = 0;
    jboolean native = 0;
    jboolean synthetic = 0;
    jint locals = 0;
    jint args = 0;
    jlocation start = 0;
    jlocation end = 0;
    jint lines = 0;
    jvmtiLineNumberEntry *table = NULL;
    jint size = 0;
    unsigned char *bytes = NULL;
    int nerr, derr, merr, naerr, serr, lerr, aerr, locerr, lnerr, berr;
    nerr = (int)(*jvmti)->GetMethodName(jvmti, m, &name, &sig, &generic);
    derr = (int)(*jvmti)->GetMethodDeclaringClass(jvmti, m, &declaring);
    merr = (int)(*jvmti)->GetMethodModifiers(jvmti, m, &modifiers);
    naerr = (int)(*jvmti)->IsMethodNative(jvmti, m, &native);
    serr = (int)(*jvmti)->IsMethodSynthetic(jvmti, m, &synthetic);
    lerr = (int)(*jvmti)->GetMaxLocals(jvmti, m, &locals);
    aerr = (int)(*jvmti)->GetArgumentsSize(jvmti, m, &args);
    locerr = (int)(*jvmti)->GetMethodLocation(jvmti, m, &start, &end);
    lnerr = (int)(*jvmti)->GetLineNumberTable(jvmti, m, &lines, &table);
    berr = (int)(*jvmti)->GetBytecodes(jvmti, m, &size, &bytes);
    line("  [%d] name err=%d %s%s generic=%s", i, nerr, nerr == 0 ? name : "-",
         nerr == 0 ? sig : "", generic == NULL ? "null" : "set");
    line("  [%d] declaring err=%d same=%d modifiers err=%d 0x%x native err=%d %d synthetic err=%d %d",
         i, derr, derr == 0 ? (int)(*env)->IsSameObject(env, declaring, klass) : -1,
         merr, (unsigned)modifiers, naerr, (int)native, serr, (int)synthetic);
    line("  [%d] locals err=%d %d args err=%d %d location err=%d %lld..%lld lines err=%d %d bytecodes err=%d %d first=%d",
         i, lerr, (int)locals, aerr, (int)args, locerr, (long long)start, (long long)end,
         lnerr, (int)lines, berr, (int)size, (berr == 0 && size > 0) ? (int)bytes[0] : -1);
    (*jvmti)->Deallocate(jvmti, (unsigned char *)name);
    (*jvmti)->Deallocate(jvmti, (unsigned char *)sig);
    (*jvmti)->Deallocate(jvmti, (unsigned char *)generic);
    (*jvmti)->Deallocate(jvmti, (unsigned char *)table);
    (*jvmti)->Deallocate(jvmti, bytes);
}

JNIEXPORT jstring JNICALL
Java_L1W56JvmtiLambdaMethods_methods(JNIEnv *env, jclass self, jclass klass)
{
    jvmtiEnv *jvmti = agent_env;
    jint count = 0;
    jmethodID *methods = NULL;
    jstring answer;
    int err;
    jint i;
    (void)self;
    if (jvmti == NULL) {
        return (*env)->NewStringUTF(env, "no JVMTI env (load the shim with -agentpath too)\n");
    }
    err = (int)(*jvmti)->GetClassMethods(jvmti, klass, &count, &methods);
    line("methods: err=%d count=%d", err, (int)count);
    for (i = 0; err == 0 && i < count; i++) {
        method_row(env, jvmti, klass, (int)i, methods[i]);
    }
    (*jvmti)->Deallocate(jvmti, (unsigned char *)methods);
    answer = (*env)->NewStringUTF(env, out);
    out[0] = '\0';
    return answer;
}

/* The frames 1..3 under this native: each `Class.method` (the class's
 * signature masked). */
JNIEXPORT jstring JNICALL
Java_L1W56JvmtiLambdaMethods_stack(JNIEnv *env, jclass self)
{
    jvmtiEnv *jvmti = agent_env;
    jvmtiFrameInfo frames[4];
    jint count = 0;
    jstring answer;
    int err;
    jint i;
    (void)self;
    if (jvmti == NULL) {
        return (*env)->NewStringUTF(env, "no JVMTI env (load the shim with -agentpath too)\n");
    }
    err = (int)(*jvmti)->GetStackTrace(jvmti, NULL, 0, 4, frames, &count);
    line("stack: err=%d count=%d", err, (int)count);
    for (i = 1; err == 0 && i < count; i++) {
        jclass declaring = NULL;
        char *csig = NULL;
        char *name = NULL;
        char masked[512];
        (*jvmti)->GetMethodDeclaringClass(jvmti, frames[i].method, &declaring);
        (*jvmti)->GetClassSignature(jvmti, declaring, &csig, NULL);
        (*jvmti)->GetMethodName(jvmti, frames[i].method, &name, NULL, NULL);
        mask(csig == NULL ? "?" : csig, masked, sizeof masked);
        line("  frame %d: %s %s", (int)i, masked, name == NULL ? "?" : name);
        (*jvmti)->Deallocate(jvmti, (unsigned char *)csig);
        (*jvmti)->Deallocate(jvmti, (unsigned char *)name);
    }
    answer = (*env)->NewStringUTF(env, out);
    out[0] = '\0';
    return answer;
}
