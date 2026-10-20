// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 119, lane L1: the agent of
// L1W119RedefineHookOrder.java (see there). Loaded both as an agent
// (`-agentpath:`) and with `System.load` (for the natives).
//
//   Windows (x64 Native Tools prompt):
//     cl /nologo /LD /I"%JAVA_HOME%\include" /I"%JAVA_HOME%\include\win32" L1W119RedefineHookOrder.c /Fe:%TEMP%\L1W119RedefineHookOrder.dll
//   Linux:
//     gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" -o /tmp/libl1w119.so L1W119RedefineHookOrder.c

#include <jni.h>
#include <jvmti.h>
#include <stdlib.h>
#include <string.h>

static jvmtiEnv *jvmti;
static unsigned char *bad_bytes;
static jint bad_len;
static int hook_calls;

static void JNICALL on_class_file_load_hook(jvmtiEnv *env, JNIEnv *jni, jclass being_redefined,
                                            jobject loader, const char *name, jobject domain,
                                            jint len, const unsigned char *data, jint *new_len,
                                            unsigned char **new_data) {
    (void)jni;
    (void)loader;
    (void)domain;
    (void)len;
    (void)data;
    if (being_redefined == NULL || name == NULL) {
        return;
    }
    if (strcmp(name, "L1W119RedefineHookOrder$B") != 0 &&
        strcmp(name, "L1W119RedefineHookOrder$A") != 0) {
        return;
    }
    hook_calls++;
    if (bad_bytes == NULL || strcmp(name, "L1W119RedefineHookOrder$B") != 0) {
        return;
    }
    unsigned char *out = NULL;
    if ((*env)->Allocate(env, bad_len, &out) != JVMTI_ERROR_NONE) {
        return;
    }
    memcpy(out, bad_bytes, (size_t)bad_len);
    *new_len = bad_len;
    *new_data = out;
}

JNIEXPORT jint JNICALL Agent_OnLoad(JavaVM *vm, char *options, void *reserved) {
    (void)options;
    (void)reserved;
    if ((*vm)->GetEnv(vm, (void **)&jvmti, JVMTI_VERSION_1_2) != JNI_OK) {
        return JNI_ERR;
    }
    jvmtiCapabilities caps;
    memset(&caps, 0, sizeof(caps));
    caps.can_redefine_classes = 1;
    if ((*jvmti)->AddCapabilities(jvmti, &caps) != JVMTI_ERROR_NONE) {
        return JNI_ERR;
    }
    jvmtiEventCallbacks callbacks;
    memset(&callbacks, 0, sizeof(callbacks));
    callbacks.ClassFileLoadHook = &on_class_file_load_hook;
    (*jvmti)->SetEventCallbacks(jvmti, &callbacks, (jint)sizeof(callbacks));
    (*jvmti)->SetEventNotificationMode(jvmti, JVMTI_ENABLE, JVMTI_EVENT_CLASS_FILE_LOAD_HOOK,
                                       NULL);
    return JNI_OK;
}

/// `static native void setBad(byte[] bytes)`: the class file the hook hands
/// back for `B` (null: hand back nothing).
JNIEXPORT void JNICALL Java_L1W119RedefineHookOrder_setBad(JNIEnv *env, jclass cls,
                                                           jbyteArray bytes) {
    (void)cls;
    free(bad_bytes);
    bad_bytes = NULL;
    bad_len = 0;
    if (bytes == NULL) {
        return;
    }
    bad_len = (*env)->GetArrayLength(env, bytes);
    bad_bytes = (unsigned char *)malloc((size_t)bad_len);
    (*env)->GetByteArrayRegion(env, bytes, 0, bad_len, (jbyte *)bad_bytes);
}

/// `static native int hookCalls()`: the hook's calls for A or B so far.
JNIEXPORT jint JNICALL Java_L1W119RedefineHookOrder_hookCalls(JNIEnv *env, jclass cls) {
    (void)env;
    (void)cls;
    return hook_calls;
}

/// `static native int redefine(Class a, byte[] ab, Class b, byte[] bb)`: one
/// `RedefineClasses` of both; its `jvmtiError`.
JNIEXPORT jint JNICALL Java_L1W119RedefineHookOrder_redefine(JNIEnv *env, jclass cls, jclass a,
                                                             jbyteArray ab, jclass b,
                                                             jbyteArray bb) {
    (void)cls;
    jvmtiClassDefinition defs[2];
    defs[0].klass = a;
    defs[0].class_byte_count = (*env)->GetArrayLength(env, ab);
    defs[0].class_bytes = (const unsigned char *)(*env)->GetByteArrayElements(env, ab, NULL);
    defs[1].klass = b;
    defs[1].class_byte_count = (*env)->GetArrayLength(env, bb);
    defs[1].class_bytes = (const unsigned char *)(*env)->GetByteArrayElements(env, bb, NULL);
    jvmtiError err = (*jvmti)->RedefineClasses(jvmti, 2, defs);
    (*env)->ReleaseByteArrayElements(env, ab, (jbyte *)defs[0].class_bytes, JNI_ABORT);
    (*env)->ReleaseByteArrayElements(env, bb, (jbyte *)defs[1].class_bytes, JNI_ABORT);
    return (jint)err;
}
