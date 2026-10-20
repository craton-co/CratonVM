/*
 * Native shim of tools/probes/interp/L6/L6W84JniAttachedThreads.java: start a
 * host pthread that attaches to the VM in one of five shapes, calls the probe's
 * static `attached()` once, and detaches. The Java side waits for the detach
 * through `Thread.join()` on the attached thread's own `Thread` object.
 *
 *   mode 0  AttachCurrentThread(NULL args)
 *   mode 1  AttachCurrentThreadAsDaemon(NULL args)
 *   mode 2  AttachCurrentThread({JNI_VERSION_1_2, "named", NULL})
 *   mode 3  AttachCurrentThread({JNI_VERSION_1_2, "", NULL})
 *   mode 4  AttachCurrentThread({0x00010001 (1.1), "ignored", NULL})
 */
#include <jni.h>
#include <pthread.h>
#include <stdlib.h>

static JavaVM *jvm;
static jclass probe_cls;

static void *run(void *p) {
    int mode = *(int *)p;
    JNIEnv *env = NULL;
    JavaVMAttachArgs a;
    void *args = NULL;
    jint rc;
    jmethodID m;
    free(p);
    a.group = NULL;
    if (mode == 2) { a.version = JNI_VERSION_1_2; a.name = (char *)"named"; args = &a; }
    if (mode == 3) { a.version = JNI_VERSION_1_2; a.name = (char *)""; args = &a; }
    if (mode == 4) { a.version = 0x00010001; a.name = (char *)"ignored"; args = &a; }
    rc = mode == 1
        ? (*jvm)->AttachCurrentThreadAsDaemon(jvm, (void **)&env, args)
        : (*jvm)->AttachCurrentThread(jvm, (void **)&env, args);
    if (rc != JNI_OK) {
        return NULL;
    }
    m = (*env)->GetStaticMethodID(env, probe_cls, "attached", "()V");
    if (m != NULL) {
        (*env)->CallStaticVoidMethod(env, probe_cls, m);
    }
    if ((*env)->ExceptionCheck(env)) {
        (*env)->ExceptionClear(env);
    }
    (*jvm)->DetachCurrentThread(jvm);
    return NULL;
}

JNIEXPORT void JNICALL Java_L6W84JniAttachedThreads_runAttached(JNIEnv *env, jclass cls, jint mode) {
    pthread_t t;
    int *m = malloc(sizeof *m);
    (*env)->GetJavaVM(env, &jvm);
    if (probe_cls == NULL) {
        probe_cls = (jclass)(*env)->NewGlobalRef(env, cls);
    }
    *m = mode;
    if (pthread_create(&t, NULL, run, m) == 0) {
        pthread_detach(t);
    }
}
