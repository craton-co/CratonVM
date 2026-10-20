/*
 * Native shim for tools/probes/interp/L1/L1W107JniDetachPendingException.java
 * (interpreter round i5 wave 107, lane L1). tools/jvmti/run-jvmti-shims.sh
 * loads every shim as an agent too, so Agent_OnLoad exists and does nothing.
 *
 * `run` starts an OS thread that attaches through JNI AttachCurrentThread
 * under the name "l1w107-pending", calls the probe's static `fromNative()`,
 * which throws, leaves the exception pending (no ExceptionClear) and
 * detaches. HotSpot's DetachCurrentThread -> JavaThread::exit hands the
 * pending exception to Thread.dispatchUncaughtException. `say` prints
 * through this file's stdio, flushed per line, so the lines of the two
 * threads keep their order.
 */
#include <jni.h>
#include <stdio.h>
#ifdef _WIN32
#include <windows.h>
#include <process.h>
#else
#include <pthread.h>
#endif

static JavaVM *the_vm = NULL;
static jclass probe_class = NULL;
static jmethodID from_native = NULL;

JNIEXPORT jint JNICALL Agent_OnLoad(JavaVM *vm, char *options, void *reserved)
{
    (void)vm;
    (void)options;
    (void)reserved;
    return 0;
}

static void attach_throw_detach(void)
{
    JNIEnv *env = NULL;
    JavaVMAttachArgs args;
    args.version = JNI_VERSION_1_8;
    args.name = (char *)"l1w107-pending";
    args.group = NULL;
    if ((*the_vm)->AttachCurrentThread(the_vm, (void **)&env, &args) != JNI_OK) {
        printf("attach failed\n");
        fflush(stdout);
        return;
    }
    (*env)->CallStaticVoidMethod(env, probe_class, from_native);
    /* The exception stays pending across the detach, on purpose. */
    (*the_vm)->DetachCurrentThread(the_vm);
}

#ifdef _WIN32
static unsigned __stdcall body(void *arg)
{
    (void)arg;
    attach_throw_detach();
    return 0;
}
#else
static void *body(void *arg)
{
    (void)arg;
    attach_throw_detach();
    return NULL;
}
#endif

JNIEXPORT void JNICALL Java_L1W107JniDetachPendingException_say(JNIEnv *env, jclass cls, jstring s)
{
    const char *chars;
    (void)cls;
    chars = (*env)->GetStringUTFChars(env, s, NULL);
    if (chars != NULL) {
        printf("%s\n", chars);
        fflush(stdout);
        (*env)->ReleaseStringUTFChars(env, s, chars);
    }
}

JNIEXPORT jboolean JNICALL Java_L1W107JniDetachPendingException_run(JNIEnv *env, jclass cls)
{
    if ((*env)->GetJavaVM(env, &the_vm) != JNI_OK) {
        return JNI_FALSE;
    }
    probe_class = (jclass)(*env)->NewGlobalRef(env, cls);
    from_native = (*env)->GetStaticMethodID(env, cls, "fromNative", "()V");
    if (from_native == NULL) {
        return JNI_FALSE;
    }
#ifdef _WIN32
    {
        HANDLE t = (HANDLE)_beginthreadex(NULL, 0, body, NULL, 0, NULL);
        WaitForSingleObject(t, INFINITE);
        CloseHandle(t);
    }
#else
    {
        pthread_t t;
        if (pthread_create(&t, NULL, body, NULL) != 0) {
            return JNI_FALSE;
        }
        pthread_join(t, NULL);
    }
#endif
    return JNI_TRUE;
}
