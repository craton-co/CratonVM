/*
 * Native shim for tools/probes/interp/L1/L1W41JvmtiStackFromNative.java
 * (interpreter round i1 wave 41, lane L1). A plain JNI library: its one
 * native asks the VM for a JVMTI env (`GetEnv`, live phase) and reads its own
 * thread's stack with `GetFrameCount`, `GetFrameLocation` and
 * `GetStackTrace` (no capability needed). Build instructions are in the
 * probe's header.
 */
#include <jni.h>
#include <jvmti.h>
#include <stdio.h>
#include <string.h>

static void append(char *buf, size_t size, const char *text)
{
    size_t used = strlen(buf);
    if (used + 1 < size) {
        snprintf(buf + used, size - used, "%s", text);
    }
}

/* The name of `method`, or "?" (the name block is freed). */
static void method_name(jvmtiEnv *jvmti, jmethodID method, char *out, size_t size)
{
    char *name = NULL;
    if ((*jvmti)->GetMethodName(jvmti, method, &name, NULL, NULL) == JVMTI_ERROR_NONE
        && name != NULL) {
        snprintf(out, size, "%s", name);
        (*jvmti)->Deallocate(jvmti, (unsigned char *)name);
    } else {
        snprintf(out, size, "?");
    }
}

static void trace_line(jvmtiEnv *jvmti, const char *label, jint start, char *buf, size_t size)
{
    jvmtiFrameInfo frames[10];
    jint count = 0;
    char line[512];
    char name[128];
    jint i;
    jvmtiError err = (*jvmti)->GetStackTrace(jvmti, NULL, start, 10, frames, &count);
    if (err != JVMTI_ERROR_NONE) {
        snprintf(line, sizeof line, "GetStackTrace %s: error=%d\n", label, (int)err);
        append(buf, size, line);
        return;
    }
    snprintf(line, sizeof line, "GetStackTrace %s:", label);
    for (i = 0; i < count; i++) {
        method_name(jvmti, frames[i].method, name, sizeof name);
        append(line, sizeof line, " ");
        append(line, sizeof line, name);
    }
    append(line, sizeof line, "\n");
    append(buf, size, line);
}

JNIEXPORT jstring JNICALL
Java_L1W41JvmtiStackFromNative_frames(JNIEnv *env, jclass self)
{
    char buf[2048] = "";
    char line[512];
    char name[128];
    JavaVM *vm = NULL;
    jvmtiEnv *jvmti = NULL;
    jint count = -1;
    jint depth;
    jvmtiError err;
    (void)self;
    if ((*env)->GetJavaVM(env, &vm) != JNI_OK
        || (*vm)->GetEnv(vm, (void **)&jvmti, JVMTI_VERSION_1_2) != JNI_OK) {
        return (*env)->NewStringUTF(env, "no JVMTI env\n");
    }
    err = (*jvmti)->GetFrameCount(jvmti, NULL, &count);
    if (err != JVMTI_ERROR_NONE) {
        snprintf(line, sizeof line, "GetFrameCount: error=%d\n", (int)err);
    } else {
        snprintf(line, sizeof line, "GetFrameCount: %d\n", (int)count);
    }
    append(buf, sizeof buf, line);
    for (depth = 0; depth < 2; depth++) {
        jmethodID method = NULL;
        jlocation location = 0;
        jboolean is_native = JNI_FALSE;
        err = (*jvmti)->GetFrameLocation(jvmti, NULL, depth, &method, &location);
        if (err != JVMTI_ERROR_NONE) {
            snprintf(line, sizeof line, "GetFrameLocation %d: error=%d\n", (int)depth, (int)err);
            append(buf, sizeof buf, line);
            continue;
        }
        method_name(jvmti, method, name, sizeof name);
        (*jvmti)->IsMethodNative(jvmti, method, &is_native);
        if (is_native) {
            snprintf(line, sizeof line, "GetFrameLocation %d: %s native=true location=%lld\n",
                     (int)depth, name, (long long)location);
        } else {
            snprintf(line, sizeof line, "GetFrameLocation %d: %s native=false location>=0=%s\n",
                     (int)depth, name, location >= 0 ? "true" : "false");
        }
        append(buf, sizeof buf, line);
    }
    trace_line(jvmti, "0..10", 0, buf, sizeof buf);
    trace_line(jvmti, "-2..", -2, buf, sizeof buf);
    (*jvmti)->DisposeEnvironment(jvmti);
    return (*env)->NewStringUTF(env, buf);
}
