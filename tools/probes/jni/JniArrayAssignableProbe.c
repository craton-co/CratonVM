/*
 * Native shim for tools/probes/JniArrayAssignableProbe.java (gc-common w36-c).
 * Three one-line forwards to the JNI functions the probe exercises. Build
 * instructions are in the probe's class comment.
 */
#include <jni.h>

JNIEXPORT jboolean JNICALL
Java_JniArrayAssignableProbe_jniIsAssignableFrom(JNIEnv *env, jclass self, jclass sub, jclass sup)
{
    (void)self;
    return (*env)->IsAssignableFrom(env, sub, sup);
}

JNIEXPORT jboolean JNICALL
Java_JniArrayAssignableProbe_jniIsInstanceOf(JNIEnv *env, jclass self, jobject obj, jclass cls)
{
    (void)self;
    return (*env)->IsInstanceOf(env, obj, cls);
}

JNIEXPORT jclass JNICALL
Java_JniArrayAssignableProbe_jniGetObjectClass(JNIEnv *env, jclass self, jobject obj)
{
    (void)self;
    return (*env)->GetObjectClass(env, obj);
}
