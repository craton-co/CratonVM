/*
 * Native shim for tools/probes/interp/L1/L1W41JvmtiGenericSignatures.java
 * (interpreter round i1 wave 41, lane L1). A plain JNI library: it asks the
 * VM's invocation interface for a JVMTI env (`GetEnv`, live phase) and calls
 * `GetClassSignature` / `GetMethodName`, which need no capability. Build
 * instructions are in the probe's header.
 */
#include <jni.h>
#include <jvmti.h>
#include <stdio.h>

static jvmtiEnv *cached_jvmti;

static jvmtiEnv *jvmti_of(JNIEnv *env)
{
    JavaVM *vm = NULL;
    if (cached_jvmti != NULL) {
        return cached_jvmti;
    }
    if ((*env)->GetJavaVM(env, &vm) != JNI_OK) {
        return NULL;
    }
    if ((*vm)->GetEnv(vm, (void **)&cached_jvmti, JVMTI_VERSION_1_2) != JNI_OK) {
        cached_jvmti = NULL;
    }
    return cached_jvmti;
}

static void release(jvmtiEnv *jvmti, char *block)
{
    if (block != NULL) {
        (*jvmti)->Deallocate(jvmti, (unsigned char *)block);
    }
}

JNIEXPORT jstring JNICALL
Java_L1W41JvmtiGenericSignatures_classSignature(JNIEnv *env, jclass self, jclass c)
{
    char buf[1024];
    char *sig = NULL;
    char *generic = NULL;
    jvmtiError err;
    jvmtiEnv *jvmti = jvmti_of(env);
    (void)self;
    if (jvmti == NULL) {
        return (*env)->NewStringUTF(env, "no JVMTI env");
    }
    err = (*jvmti)->GetClassSignature(jvmti, c, &sig, &generic);
    if (err != JVMTI_ERROR_NONE) {
        snprintf(buf, sizeof buf, "error=%d", (int)err);
    } else {
        snprintf(buf, sizeof buf, "sig=%s generic=%s", sig ? sig : "NULL",
                 generic ? generic : "NULL");
    }
    release(jvmti, sig);
    release(jvmti, generic);
    return (*env)->NewStringUTF(env, buf);
}

JNIEXPORT jstring JNICALL
Java_L1W41JvmtiGenericSignatures_methodSignature(JNIEnv *env, jclass self, jclass c,
                                                 jstring name, jstring desc,
                                                 jboolean is_static)
{
    char buf[1024];
    char *mname = NULL;
    char *msig = NULL;
    char *generic = NULL;
    jmethodID method;
    jvmtiError err;
    const char *n;
    const char *d;
    jvmtiEnv *jvmti = jvmti_of(env);
    (void)self;
    if (jvmti == NULL) {
        return (*env)->NewStringUTF(env, "no JVMTI env");
    }
    n = (*env)->GetStringUTFChars(env, name, NULL);
    d = (*env)->GetStringUTFChars(env, desc, NULL);
    method = is_static ? (*env)->GetStaticMethodID(env, c, n, d)
                       : (*env)->GetMethodID(env, c, n, d);
    (*env)->ReleaseStringUTFChars(env, name, n);
    (*env)->ReleaseStringUTFChars(env, desc, d);
    if (method == NULL) {
        (*env)->ExceptionClear(env);
        return (*env)->NewStringUTF(env, "no method");
    }
    err = (*jvmti)->GetMethodName(jvmti, method, &mname, &msig, &generic);
    if (err != JVMTI_ERROR_NONE) {
        snprintf(buf, sizeof buf, "error=%d", (int)err);
    } else {
        snprintf(buf, sizeof buf, "name=%s sig=%s generic=%s", mname ? mname : "NULL",
                 msig ? msig : "NULL", generic ? generic : "NULL");
    }
    release(jvmti, mname);
    release(jvmti, msig);
    release(jvmti, generic);
    return (*env)->NewStringUTF(env, buf);
}
