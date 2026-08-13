/*
 * The native half of JniNativeInteropProbe.
 *
 * Two families, both taken from real third-party JNI libraries that CratonVM
 * could not run, and both written so a wrong answer is a printed VALUE rather
 * than a crash — except where the defect genuinely WAS a crash, which is the
 * point of the first family.
 *
 *   directBuffer*   GetDirectBufferAddress on a ByteBuffer.allocateDirect, then
 *                   an ordinary C load/store through the returned pointer. This
 *                   is one line of lz4-java's XXH32BB and zstd-jni's
 *                   getDirectByteBufferFrameContentSize, and on CratonVM it was
 *                   a SIGSEGV: allocateDirect runs the real JDK bytecode, which
 *                   allocates through Unsafe, and Unsafe.allocateMemory on this
 *                   VM returns a tagged arena HANDLE rather than an OS pointer.
 *                   Handing that to C is a wild pointer by construction.
 *
 *   classParam*     A java.lang.Class that arrives as an ordinary Object
 *                   PARAMETER, used where a jclass is expected. This is
 *                   barchart-udt's SocketUDT.setOption0(int, Class<?>, Object),
 *                   which dispatches on IsSameObject(klaz, <cached class>). A
 *                   class reaches native code under two different CratonVM
 *                   encodings and nothing reconciled them, so every one of these
 *                   answered "no"/NULL.
 *
 * Nothing here is CratonVM-aware. The diff against the HotSpot arm is what says
 * whether it works. Build:
 *
 *   gcc -shared -fPIC -O1 -I$JAVA_HOME/include -I$JAVA_HOME/include/linux \
 *       -o libjninativeinterop.so probes/jni_native_interop_probe.c
 */

#include <jni.h>
#include <string.h>

/* ------------------------------------------------------------------ */
/* Family 1: GetDirectBufferAddress                                     */
/* ------------------------------------------------------------------ */

/* Is the address non-NULL, and does the reported capacity agree? Returns
 * 1 both-ok, 0 address NULL, -1 capacity disagrees. Split out from the
 * load/store below so "we never got a pointer" is distinguishable from
 * "we got one and it was wrong". */
JNIEXPORT jint JNICALL
Java_JniNativeInteropProbe_directBufferAddressOk(JNIEnv *env, jclass self, jobject buf, jlong cap) {
    void *p = (*env)->GetDirectBufferAddress(env, buf);
    if (p == NULL) { return 0; }
    if ((*env)->GetDirectBufferCapacity(env, buf) != cap) { return -1; }
    return 1;
}

/* Read a byte Java wrote. The whole family SIGSEGV'd here. */
JNIEXPORT jint JNICALL
Java_JniNativeInteropProbe_directBufferRead(JNIEnv *env, jclass self, jobject buf, jint index) {
    unsigned char *p = (unsigned char *)(*env)->GetDirectBufferAddress(env, buf);
    if (p == NULL) { return -1; }
    return (jint)p[index];
}

/* Write a byte for Java to read back — the aliasing half. A GetDirectBuffer-
 * Address that handed out a COPY would pass directBufferRead and fail this. */
JNIEXPORT void JNICALL
Java_JniNativeInteropProbe_directBufferWrite(JNIEnv *env, jclass self, jobject buf, jint index, jint value) {
    unsigned char *p = (unsigned char *)(*env)->GetDirectBufferAddress(env, buf);
    if (p == NULL) { return; }
    p[index] = (unsigned char)value;
}

/* A checksum over the whole buffer, the shape the codecs actually use:
 * base pointer + offset, walked in C. */
JNIEXPORT jlong JNICALL
Java_JniNativeInteropProbe_directBufferSum(JNIEnv *env, jclass self, jobject buf, jint off, jint len) {
    unsigned char *p = (unsigned char *)(*env)->GetDirectBufferAddress(env, buf);
    jlong sum = 0;
    jint i;
    if (p == NULL) { return -1; }
    for (i = 0; i < len; i++) { sum += p[off + i]; }
    return sum;
}

/* A non-direct buffer must answer NULL — the JNI spec's own sentinel, and the
 * one thing a caller may legitimately test for. */
JNIEXPORT jint JNICALL
Java_JniNativeInteropProbe_heapBufferAddressIsNull(JNIEnv *env, jclass self, jobject buf) {
    return (*env)->GetDirectBufferAddress(env, buf) == NULL ? 1 : 0;
}

/* GetDirectBufferCapacity's own answer for a non-direct buffer is -1. Probed
 * separately because a heap buffer HAS a real `capacity` field, so a VM that
 * refuses the address but answers the capacity is self-contradictory. */
JNIEXPORT jlong JNICALL
Java_JniNativeInteropProbe_heapBufferCapacity(JNIEnv *env, jclass self, jobject buf) {
    return (*env)->GetDirectBufferCapacity(env, buf);
}

/* ------------------------------------------------------------------ */
/* Family 2: a Class arriving as a parameter                            */
/* ------------------------------------------------------------------ */

static jclass find(JNIEnv *env, jstring name) {
    const char *n = (*env)->GetStringUTFChars(env, name, 0);
    jclass c = (*env)->FindClass(env, n);
    (*env)->ReleaseStringUTFChars(env, name, n);
    return c;
}

JNIEXPORT jint JNICALL
Java_JniNativeInteropProbe_classParamSameAsFindClass(JNIEnv *env, jclass self, jobject klaz, jstring name) {
    jclass found = find(env, name);
    if (found == NULL) { return -1; }
    return (*env)->IsSameObject(env, klaz, found) ? 1 : 0;
}

/* Through a global ref — what a library caches in its JNI_OnLoad / initIDs. */
JNIEXPORT jint JNICALL
Java_JniNativeInteropProbe_classParamSameAsGlobalRef(JNIEnv *env, jclass self, jobject klaz, jstring name) {
    jclass found = find(env, name);
    jobject g;
    jint r;
    if (found == NULL) { return -1; }
    g = (*env)->NewGlobalRef(env, found);
    if (g == NULL) { return -2; }
    r = (*env)->IsSameObject(env, klaz, g) ? 1 : 0;
    (*env)->DeleteGlobalRef(env, g);
    return r;
}

JNIEXPORT jint JNICALL
Java_JniNativeInteropProbe_classParamMethodId(JNIEnv *env, jclass self, jobject klaz) {
    jmethodID mid = (*env)->GetMethodID(env, (jclass)klaz, "toString", "()Ljava/lang/String;");
    if ((*env)->ExceptionCheck(env)) { (*env)->ExceptionClear(env); }
    return mid != NULL ? 1 : 0;
}

JNIEXPORT jint JNICALL
Java_JniNativeInteropProbe_classParamStaticFieldId(JNIEnv *env, jclass self, jobject klaz) {
    jfieldID fid = (*env)->GetStaticFieldID(env, (jclass)klaz, "TRUE", "Ljava/lang/Boolean;");
    if ((*env)->ExceptionCheck(env)) { (*env)->ExceptionClear(env); }
    return fid != NULL ? 1 : 0;
}

JNIEXPORT jint JNICALL
Java_JniNativeInteropProbe_classParamAssignable(JNIEnv *env, jclass self, jobject sub, jobject sup) {
    jint r = (*env)->IsAssignableFrom(env, (jclass)sub, (jclass)sup) ? 1 : 0;
    if ((*env)->ExceptionCheck(env)) { (*env)->ExceptionClear(env); }
    return r;
}

JNIEXPORT jint JNICALL
Java_JniNativeInteropProbe_classParamIsInstance(JNIEnv *env, jclass self, jobject value, jobject klaz) {
    jint r = (*env)->IsInstanceOf(env, value, (jclass)klaz) ? 1 : 0;
    if ((*env)->ExceptionCheck(env)) { (*env)->ExceptionClear(env); }
    return r;
}

/* The reverse direction, which a fix that RE-ENCODED Class parameters as
 * jclass handles would have broken: the same reference must still work as an
 * ordinary object, i.e. as the receiver of a virtual call. */
JNIEXPORT jstring JNICALL
Java_JniNativeInteropProbe_classParamGetName(JNIEnv *env, jclass self, jobject klaz) {
    jclass cls = (*env)->FindClass(env, "java/lang/Class");
    jmethodID mid;
    jvalue noargs[1];
    if (cls == NULL) { return (*env)->NewStringUTF(env, "<no java/lang/Class>"); }
    mid = (*env)->GetMethodID(env, cls, "getName", "()Ljava/lang/String;");
    if (mid == NULL) { return (*env)->NewStringUTF(env, "<no getName>"); }
    return (jstring)(*env)->CallObjectMethodA(env, klaz, mid, noargs);
}

/* And that it still round-trips back into Java as a Class. */
JNIEXPORT jobject JNICALL
Java_JniNativeInteropProbe_classParamEcho(JNIEnv *env, jclass self, jobject klaz) {
    return klaz;
}
