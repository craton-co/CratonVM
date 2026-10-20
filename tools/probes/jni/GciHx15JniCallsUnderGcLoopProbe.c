/*
 * gci wave i15, lane hx: the natives of
 * tools/probes/GciHx15JniCallsUnderGcLoopProbe.java (thousands of JNIEnv calls
 * per native invocation while collections are requested back to back).
 *
 *   Windows: cl /LD /O1 /I"%JDK%\include" /I"%JDK%\include\win32" GciHx15JniCallsUnderGcLoopProbe.c /Fegcihx15jni.dll
 *   Linux:   gcc -shared -fPIC -O1 -I$JDK/include -I$JDK/include/linux -o libgcihx15jni.so GciHx15JniCallsUnderGcLoopProbe.c
 */
#include <jni.h>

static jfieldID value_fid;

JNIEXPORT jint JNICALL Java_GciHx15JniCallsUnderGcLoopProbe_init(JNIEnv *env, jclass cls, jclass node) {
    (void)cls;
    value_fid = (*env)->GetFieldID(env, node, "value", "I");
    return value_fid == NULL ? -2 : 0;
}

JNIEXPORT jlong JNICALL Java_GciHx15JniCallsUnderGcLoopProbe_sumAll(JNIEnv *env, jclass cls, jobjectArray objs) {
    jint n;
    jint i;
    jlong sum = 0;
    (void)cls;
    n = (*env)->GetArrayLength(env, objs);
    for (i = 0; i < n; i++) {
        jobject o = (*env)->GetObjectArrayElement(env, objs, i);
        if (o == NULL) {
            return -1;
        }
        sum += (*env)->GetIntField(env, o, value_fid);
        (*env)->DeleteLocalRef(env, o);
    }
    return sum;
}
