/*
 * Native shim for tools/probes/interp/L1/L1W43JvmtiNativeBelowCompiledUpcall.java
 * (interpreter round i1 wave 43, lane L1): the shim of
 * L1W42JvmtiNativeRows.c under this class name, with the JVMTI env obtained
 * once and kept (the probe calls `frames()` tens of thousands of times).
 * `frames()` reads its own thread's stack with `GetFrameCount` and
 * `GetStackTrace` (no capability needed); `nested()` calls the static Java
 * method `up()` back and returns its answer. Build instructions are in the
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

JNIEXPORT jstring JNICALL
Java_L1W43JvmtiNativeBelowCompiledUpcall_frames(JNIEnv *env, jclass self)
{
    char buf[1024] = "";
    char item[160];
    static jvmtiEnv *jvmti = NULL;
    JavaVM *vm = NULL;
    jvmtiFrameInfo frames[16];
    jint count = -1;
    jint n = 0;
    jint i;
    jvmtiError err;
    (void)self;
    if (jvmti == NULL
        && ((*env)->GetJavaVM(env, &vm) != JNI_OK
            || (*vm)->GetEnv(vm, (void **)&jvmti, JVMTI_VERSION_1_2) != JNI_OK)) {
        jvmti = NULL;
        return (*env)->NewStringUTF(env, "no JVMTI env");
    }
    err = (*jvmti)->GetFrameCount(jvmti, NULL, &count);
    if (err != JVMTI_ERROR_NONE) {
        snprintf(item, sizeof item, "count=error%d", (int)err);
    } else {
        snprintf(item, sizeof item, "count=%d", (int)count);
    }
    append(buf, sizeof buf, item);
    err = (*jvmti)->GetStackTrace(jvmti, NULL, 0, 16, frames, &n);
    if (err != JVMTI_ERROR_NONE) {
        snprintf(item, sizeof item, " trace=error%d", (int)err);
        append(buf, sizeof buf, item);
        n = 0;
    }
    for (i = 0; i < n; i++) {
        char *name = NULL;
        if ((*jvmti)->GetMethodName(jvmti, frames[i].method, &name, NULL, NULL) == JVMTI_ERROR_NONE
            && name != NULL) {
            snprintf(item, sizeof item, " %s%s", name, frames[i].location == -1 ? "@-1" : "");
            (*jvmti)->Deallocate(jvmti, (unsigned char *)name);
        } else {
            snprintf(item, sizeof item, " ?");
        }
        append(buf, sizeof buf, item);
    }
    return (*env)->NewStringUTF(env, buf);
}

JNIEXPORT jstring JNICALL
Java_L1W43JvmtiNativeBelowCompiledUpcall_nested(JNIEnv *env, jclass self)
{
    jmethodID up = (*env)->GetStaticMethodID(env, self, "up", "()Ljava/lang/String;");
    if (up == NULL) {
        return (*env)->NewStringUTF(env, "no up()");
    }
    return (jstring)(*env)->CallStaticObjectMethod(env, self, up);
}
