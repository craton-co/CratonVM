/*
 * The native half of JdkOnlyPlatformProbe's `jni` section.
 *
 * Built by scripts/jdk-only-strict-probes.sh and handed to every arm via
 * -Dcraton.probe.jnilib=<path>, so HotSpot and both CratonVM policies run the
 * SAME shared object. Nothing here is CratonVM-aware: if a family is broken in
 * strict mode, the diff against the HotSpot arm is what says so.
 *
 * The families are deliberately one per function, because "JNI works" is not a
 * measurement:
 *
 *   add/mulLong/scale     the primitive call ABI, including 64-bit and double
 *                         returns, which is where a wrong calling convention
 *                         shows up as a wrong VALUE rather than a crash
 *   reverse               GetStringUTFChars / NewStringUTF
 *   sumInts/doubleInts    Get<Primitive>ArrayElements + the commit-back mode,
 *                         which is the half that is silently dropped when the
 *                         array is copied instead of pinned
 *   joinStrings           GetObjectArrayElement over an object array
 *   readValue/writeValue  GetFieldID + Get/SetIntField on an instance
 *   callBackTriple        GetStaticMethodID + CallStaticIntMethod: native ->
 *                         Java re-entry
 *   upcallThrow           the other half of that re-entry: when the Java method
 *                         THROWS, the exception must be pending at the up-call's
 *                         return so the native can see it -- a native that reads
 *                         the return value with nothing pending cannot tell a
 *                         thrown exception from a real 0
 *   throwIse              ThrowNew, and that a pending exception propagates to
 *                         the Java caller rather than being swallowed
 *   registeredNative      bound by JNI_OnLoad/RegisterNatives instead of by
 *                         symbol lookup -- a completely separate resolution
 *                         path in the VM
 *
 * The class is a NESTED class, so the mangled prefix carries the _00024 escape
 * for '$'. A mismatch there surfaces as UnsatisfiedLinkError on the first call,
 * which the probe prints rather than aborting on.
 */
#include <jni.h>
#include <string.h>
#include <stdlib.h>

#define PFX Java_JdkOnlyPlatformProbe_00024JniProbe_

#define CAT_(a, b) a##b
#define CAT(a, b) CAT_(a, b)
#define FN(name) CAT(PFX, name)

JNIEXPORT jint JNICALL FN(add)(JNIEnv *env, jclass cls, jint a, jint b) {
    (void) env; (void) cls;
    return a + b;
}

JNIEXPORT jlong JNICALL FN(mulLong)(JNIEnv *env, jclass cls, jlong a, jlong b) {
    (void) env; (void) cls;
    return a * b;
}

JNIEXPORT jdouble JNICALL FN(scale)(JNIEnv *env, jclass cls, jdouble d, jint by) {
    (void) env; (void) cls;
    return d * (jdouble) by;
}

JNIEXPORT jstring JNICALL FN(reverse)(JNIEnv *env, jclass cls, jstring s) {
    const char *in;
    char *out;
    jsize n, i;
    jstring result;
    (void) cls;

    if (s == NULL) return NULL;
    in = (*env)->GetStringUTFChars(env, s, NULL);
    if (in == NULL) return NULL;
    n = (jsize) strlen(in);
    out = (char *) malloc((size_t) n + 1);
    if (out == NULL) {
        (*env)->ReleaseStringUTFChars(env, s, in);
        return NULL;
    }
    for (i = 0; i < n; i++) out[i] = in[n - 1 - i];
    out[n] = '\0';
    (*env)->ReleaseStringUTFChars(env, s, in);
    result = (*env)->NewStringUTF(env, out);
    free(out);
    return result;
}

JNIEXPORT jint JNICALL FN(sumInts)(JNIEnv *env, jclass cls, jintArray a) {
    jint *body;
    jsize n, i;
    jint sum = 0;
    (void) cls;

    if (a == NULL) return -1;
    n = (*env)->GetArrayLength(env, a);
    body = (*env)->GetIntArrayElements(env, a, NULL);
    if (body == NULL) return -1;
    for (i = 0; i < n; i++) sum += body[i];
    /* JNI_ABORT: no write-back. The caller checks the array is unchanged by
     * this call and changed by doubleInts, which is the only way to tell a
     * commit from a no-op when the buffer happened to be pinned. */
    (*env)->ReleaseIntArrayElements(env, a, body, JNI_ABORT);
    return sum;
}

JNIEXPORT void JNICALL FN(doubleInts)(JNIEnv *env, jclass cls, jintArray a) {
    jint *body;
    jsize n, i;
    (void) cls;

    if (a == NULL) return;
    n = (*env)->GetArrayLength(env, a);
    body = (*env)->GetIntArrayElements(env, a, NULL);
    if (body == NULL) return;
    for (i = 0; i < n; i++) body[i] *= 2;
    (*env)->ReleaseIntArrayElements(env, a, body, 0);
}

JNIEXPORT jstring JNICALL FN(joinStrings)(JNIEnv *env, jclass cls, jobjectArray a) {
    jsize n, i;
    char buf[256];
    size_t used = 0;
    (void) cls;

    buf[0] = '\0';
    if (a == NULL) return (*env)->NewStringUTF(env, "");
    n = (*env)->GetArrayLength(env, a);
    for (i = 0; i < n; i++) {
        jstring el = (jstring) (*env)->GetObjectArrayElement(env, a, i);
        const char *s;
        size_t len;
        if (el == NULL) continue;
        s = (*env)->GetStringUTFChars(env, el, NULL);
        if (s == NULL) { (*env)->DeleteLocalRef(env, el); continue; }
        len = strlen(s);
        if (used + len + 2 < sizeof buf) {
            if (used > 0) buf[used++] = '|';
            memcpy(buf + used, s, len);
            used += len;
            buf[used] = '\0';
        }
        (*env)->ReleaseStringUTFChars(env, el, s);
        (*env)->DeleteLocalRef(env, el);
    }
    return (*env)->NewStringUTF(env, buf);
}

static jfieldID value_field(JNIEnv *env, jobject o) {
    jclass k = (*env)->GetObjectClass(env, o);
    jfieldID f;
    if (k == NULL) return NULL;
    f = (*env)->GetFieldID(env, k, "value", "I");
    (*env)->DeleteLocalRef(env, k);
    return f;
}

JNIEXPORT jint JNICALL FN(readValue)(JNIEnv *env, jclass cls, jobject o) {
    jfieldID f;
    (void) cls;
    if (o == NULL) return -1;
    f = value_field(env, o);
    if (f == NULL) return -1;
    return (*env)->GetIntField(env, o, f);
}

JNIEXPORT void JNICALL FN(writeValue)(JNIEnv *env, jclass cls, jobject o, jint v) {
    jfieldID f;
    (void) cls;
    if (o == NULL) return;
    f = value_field(env, o);
    if (f == NULL) return;
    (*env)->SetIntField(env, o, f, v);
}

JNIEXPORT void JNICALL FN(throwIse)(JNIEnv *env, jclass cls, jstring msg) {
    jclass ise;
    const char *m = NULL;
    (void) cls;

    ise = (*env)->FindClass(env, "java/lang/IllegalStateException");
    if (ise == NULL) return;
    if (msg != NULL) m = (*env)->GetStringUTFChars(env, msg, NULL);
    (*env)->ThrowNew(env, ise, m != NULL ? m : "");
    if (m != NULL) (*env)->ReleaseStringUTFChars(env, msg, m);
    (*env)->DeleteLocalRef(env, ise);
}

JNIEXPORT jstring JNICALL FN(upcallThrow)(JNIEnv *env, jclass cls, jint n) {
    jmethodID mid;
    jthrowable pending;
    jclass iae;
    const char *verdict;

    mid = (*env)->GetStaticMethodID(env, cls, "boom", "(I)I");
    if (mid == NULL) return (*env)->NewStringUTF(env, "no-mid");
    (void) (*env)->CallStaticIntMethod(env, cls, mid, n);
    if (!(*env)->ExceptionCheck(env)) {
        return (*env)->NewStringUTF(env, "no-pending");
    }
    pending = (*env)->ExceptionOccurred(env);
    (*env)->ExceptionClear(env);
    if (pending == NULL) {
        return (*env)->NewStringUTF(env, "pending-but-null");
    }
    iae = (*env)->FindClass(env, "java/lang/IllegalArgumentException");
    if (iae != NULL && (*env)->IsInstanceOf(env, pending, iae)) {
        verdict = "iae";
    } else {
        verdict = "other";
    }
    if (iae != NULL) (*env)->DeleteLocalRef(env, iae);
    /* The clear above must have taken: this native returns normally, so a
     * still-pending exception would surface in the CALLER rather than here. */
    if ((*env)->ExceptionCheck(env)) {
        (*env)->DeleteLocalRef(env, pending);
        return (*env)->NewStringUTF(env, "not-cleared");
    }
    (*env)->DeleteLocalRef(env, pending);
    return (*env)->NewStringUTF(env, verdict);
}

JNIEXPORT jint JNICALL FN(callBackTriple)(JNIEnv *env, jclass cls, jint n) {
    jmethodID mid = (*env)->GetStaticMethodID(env, cls, "triple", "(I)I");
    if (mid == NULL) return -1;
    return (*env)->CallStaticIntMethod(env, cls, mid, n);
}

/* Bound through RegisterNatives below, NOT by its exported symbol name --
 * the C name is deliberately unrelated to the Java one so a VM that only
 * does dlsym-by-mangled-name fails this and nothing else. */
static jint JNICALL registered_impl(JNIEnv *env, jclass cls, jint n) {
    (void) env; (void) cls;
    return n * 10;
}

JNIEXPORT jint JNICALL JNI_OnLoad(JavaVM *vm, void *reserved) {
    JNIEnv *env = NULL;
    jclass cls;
    JNINativeMethod m;
    (void) reserved;

    if ((*vm)->GetEnv(vm, (void **) &env, JNI_VERSION_1_8) != JNI_OK || env == NULL) {
        return JNI_VERSION_1_8;
    }
    cls = (*env)->FindClass(env, "JdkOnlyPlatformProbe$JniProbe");
    if (cls == NULL) {
        /* Leave the pending NoClassDefFoundError cleared: the probe reports
         * the resulting UnsatisfiedLinkError from Java, which is a readable
         * failure, whereas an exception pending across JNI_OnLoad is not. */
        (*env)->ExceptionClear(env);
        return JNI_VERSION_1_8;
    }
    m.name = (char *) "registeredNative";
    m.signature = (char *) "(I)I";
    m.fnPtr = (void *) registered_impl;
    if ((*env)->RegisterNatives(env, cls, &m, 1) != JNI_OK) {
        (*env)->ExceptionClear(env);
    }
    (*env)->DeleteLocalRef(env, cls);
    return JNI_VERSION_1_8;
}
