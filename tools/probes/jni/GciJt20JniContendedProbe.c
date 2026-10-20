/*
 * gci wave i20, lane jt: the natives of tools/probes/GciJt20JniContendedProbe.java
 * (T Java threads calling JNI natives in a loop at once: the contended price of
 * the in-native bracket; docs/known-issues/gc/gci-i19jt-jni-calls-serialise-on-the-barrier-lock-20261014.md).
 *
 *   Linux:   gcc -shared -fPIC -O1 -I$JDK/include -I$JDK/include/linux -o libgcijt20jni.so GciJt20JniContendedProbe.c
 *   Windows: cl /LD /O1 /I"%JDK%\include" /I"%JDK%\include\win32" GciJt20JniContendedProbe.c /Fegcijt20jni.dll
 */
#include <jni.h>

/* The empty native: the dispatch and the in-native bracket, nothing else. */
JNIEXPORT jint JNICALL
Java_GciJt20JniContendedProbe_noop(JNIEnv *env, jclass self, jint x)
{
    (void)env;
    (void)self;
    return x + 1;
}

/* A native whose argument is a fresh (young, movable) array: two leaf JNIEnv
 * calls read it. Returns a[0] * 2 + length, or -1 when the region read fails. */
JNIEXPORT jint JNICALL
Java_GciJt20JniContendedProbe_arrayPeek(JNIEnv *env, jclass self, jintArray a)
{
    jint first = 0;
    jint len;
    (void)self;
    len = (*env)->GetArrayLength(env, a);
    if (len < 1) {
        return -1;
    }
    (*env)->GetIntArrayRegion(env, a, 0, 1, &first);
    if ((*env)->ExceptionCheck(env)) {
        return -1;
    }
    return first * 2 + len;
}

/* A native that makes NON-leaf JNIEnv calls (it allocates and mints a local):
 * each leaves native and comes back. Returns x + the string's UTF length (4),
 * or -1 when the allocation fails. */
JNIEXPORT jint JNICALL
Java_GciJt20JniContendedProbe_newString(JNIEnv *env, jclass self, jint x)
{
    jstring s;
    jint len;
    (void)self;
    s = (*env)->NewStringUTF(env, "jt20");
    if (s == NULL) {
        return -1;
    }
    len = (*env)->GetStringUTFLength(env, s);
    (*env)->DeleteLocalRef(env, s);
    return x + len;
}
