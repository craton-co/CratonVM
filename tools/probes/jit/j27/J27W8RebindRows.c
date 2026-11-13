/*
 * Native shim for tools/probes/jit/j27/J27W8RebindRows.java (j27 wave 8, lane
 * W8DSP, page j27-jni-registernatives-platform-rebind-warning-missing-20261028.md).
 * `rebind(Class, String name, String sig)` calls RegisterNatives with one
 * entry and answers the pending throwable (cleared) or an Integer of the
 * return code. `()V` binds a no-op, `()I` a function answering 7; any other
 * signature binds the no-op (only used for entries HotSpot refuses).
 * Build instructions are in the probe's header.
 */
#include <jni.h>
#include <string.h>

static void JNICALL noop_v(JNIEnv *env, jclass cls)
{
    (void)env;
    (void)cls;
}

static jint JNICALL ret7(JNIEnv *env, jclass cls)
{
    (void)env;
    (void)cls;
    return 7;
}

JNIEXPORT jobject JNICALL
Java_J27W8RebindRows_rebind(JNIEnv *env, jclass self, jclass target, jstring name, jstring sig)
{
    const char *n = (*env)->GetStringUTFChars(env, name, NULL);
    const char *s = (*env)->GetStringUTFChars(env, sig, NULL);
    JNINativeMethod m;
    jint rc;
    jthrowable pending;
    jclass integer;
    jmethodID value_of;
    (void)self;
    m.name = (char *)n;
    m.signature = (char *)s;
    m.fnPtr = strcmp(s, "()I") == 0 ? (void *)ret7 : (void *)noop_v;
    rc = (*env)->RegisterNatives(env, target, &m, 1);
    pending = (*env)->ExceptionOccurred(env);
    if (pending != NULL) {
        (*env)->ExceptionClear(env);
    }
    (*env)->ReleaseStringUTFChars(env, name, n);
    (*env)->ReleaseStringUTFChars(env, sig, s);
    if (pending != NULL) {
        return pending;
    }
    integer = (*env)->FindClass(env, "java/lang/Integer");
    value_of = (*env)->GetStaticMethodID(env, integer, "valueOf", "(I)Ljava/lang/Integer;");
    return (*env)->CallStaticObjectMethod(env, integer, value_of, rc);
}
