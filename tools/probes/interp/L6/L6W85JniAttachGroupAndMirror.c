/*
 * Native shim of tools/probes/interp/L6/L6W85JniAttachGroupAndMirror.java.
 *
 *   attachInGroup(g)    a pthread attaches with {JNI_VERSION_1_2, NULL, g},
 *                       calls the probe's static attached(), detaches
 *   attachNamed(bytes)  the same with the name given as raw (modified UTF-8)
 *                       bytes and no group
 *   attachAndHold(n)    n pthreads attach with no args and wait, without
 *                       calling Java, until release(); then detach
 *   attachedCount()     how many attachAndHold threads have attached
 */
#include <jni.h>
#include <pthread.h>
#include <stdlib.h>
#include <string.h>

static JavaVM *jvm;
static jclass probe_cls;
static pthread_mutex_t mu = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t cv = PTHREAD_COND_INITIALIZER;
static int attached_count;
static int released;

struct job {
    int hold;
    jobject group;  /* global ref or NULL */
    char *name;     /* malloc'd, or NULL */
};

static void call_attached(JNIEnv *env) {
    jmethodID m = (*env)->GetStaticMethodID(env, probe_cls, "attached", "()V");
    if (m != NULL) {
        (*env)->CallStaticVoidMethod(env, probe_cls, m);
    }
    if ((*env)->ExceptionCheck(env)) {
        (*env)->ExceptionClear(env);
    }
}

static void *run(void *p) {
    struct job *j = p;
    JNIEnv *env = NULL;
    JavaVMAttachArgs a;
    a.version = JNI_VERSION_1_2;
    a.name = j->name;
    a.group = j->group;
    if ((*jvm)->AttachCurrentThread(jvm, (void **)&env, j->hold ? NULL : &a) == JNI_OK) {
        if (j->hold) {
            pthread_mutex_lock(&mu);
            attached_count++;
            while (!released) {
                pthread_cond_wait(&cv, &mu);
            }
            pthread_mutex_unlock(&mu);
        } else {
            call_attached(env);
        }
        if (j->group != NULL) {
            (*env)->DeleteGlobalRef(env, j->group);
        }
        (*jvm)->DetachCurrentThread(jvm);
    }
    free(j->name);
    free(j);
    return NULL;
}

static void start(JNIEnv *env, jclass cls, struct job *j) {
    pthread_t t;
    (*env)->GetJavaVM(env, &jvm);
    if (probe_cls == NULL) {
        probe_cls = (jclass)(*env)->NewGlobalRef(env, cls);
    }
    if (pthread_create(&t, NULL, run, j) == 0) {
        pthread_detach(t);
    }
}

JNIEXPORT void JNICALL Java_L6W85JniAttachGroupAndMirror_attachInGroup(JNIEnv *env, jclass cls, jobject g) {
    struct job *j = calloc(1, sizeof *j);
    j->group = (*env)->NewGlobalRef(env, g);
    start(env, cls, j);
}

JNIEXPORT void JNICALL Java_L6W85JniAttachGroupAndMirror_attachNamed(JNIEnv *env, jclass cls, jbyteArray bytes) {
    struct job *j = calloc(1, sizeof *j);
    jsize n = (*env)->GetArrayLength(env, bytes);
    j->name = calloc((size_t)n + 1, 1);
    (*env)->GetByteArrayRegion(env, bytes, 0, n, (jbyte *)j->name);
    start(env, cls, j);
}

JNIEXPORT void JNICALL Java_L6W85JniAttachGroupAndMirror_attachAndHold(JNIEnv *env, jclass cls, jint n) {
    int i;
    for (i = 0; i < n; i++) {
        struct job *j = calloc(1, sizeof *j);
        j->hold = 1;
        start(env, cls, j);
    }
}

JNIEXPORT jint JNICALL Java_L6W85JniAttachGroupAndMirror_attachedCount(JNIEnv *env, jclass cls) {
    int n;
    pthread_mutex_lock(&mu);
    n = attached_count;
    pthread_mutex_unlock(&mu);
    return n;
}

JNIEXPORT void JNICALL Java_L6W85JniAttachGroupAndMirror_release(JNIEnv *env, jclass cls) {
    pthread_mutex_lock(&mu);
    released = 1;
    pthread_cond_broadcast(&cv);
    pthread_mutex_unlock(&mu);
}
