/*
 * Native shim of tools/probes/interp/L6/L6W101JniMonitorExitNotOwner.java.
 *
 *   enter(o)  MonitorEnter(o), then reports (rc, pending throwable)
 *   exit(o)   MonitorExit(o), then reports (rc, pending throwable)
 *
 * Each clears the pending throwable and hands it, with the return code, to
 * the probe's static report(int, Throwable).
 */
#include <jni.h>

static void report(JNIEnv *env, jclass cls, jint rc) {
    jthrowable t = (*env)->ExceptionOccurred(env);
    if (t != NULL) {
        (*env)->ExceptionClear(env);
    }
    jmethodID m = (*env)->GetStaticMethodID(env, cls, "report", "(ILjava/lang/Throwable;)V");
    (*env)->CallStaticVoidMethod(env, cls, m, rc, t);
}

JNIEXPORT void JNICALL Java_L6W101JniMonitorExitNotOwner_enter(JNIEnv *env, jclass cls, jobject o) {
    jint rc = (*env)->MonitorEnter(env, o);
    report(env, cls, rc);
}

JNIEXPORT void JNICALL Java_L6W101JniMonitorExitNotOwner_exit(JNIEnv *env, jclass cls, jobject o) {
    jint rc = (*env)->MonitorExit(env, o);
    report(env, cls, rc);
}
