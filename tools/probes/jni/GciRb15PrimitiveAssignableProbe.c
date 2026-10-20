/*
 * gci wave i15, lane rb: the native of
 * tools/probes/GciRb15PrimitiveAssignableProbe.java (JNI IsAssignableFrom over
 * Class parameters, primitive mirrors included).
 *
 *   Windows: cl /LD /O1 /I"%JDK%\include" /I"%JDK%\include\win32" GciRb15PrimitiveAssignableProbe.c /Fegcirb15asg.dll
 *   Linux:   gcc -shared -fPIC -O1 -I$JDK/include -I$JDK/include/linux -o libgcirb15asg.so GciRb15PrimitiveAssignableProbe.c
 */
#include <jni.h>

JNIEXPORT jboolean JNICALL Java_GciRb15PrimitiveAssignableProbe_assignable(JNIEnv *env, jclass cls, jclass sub,
                                                                           jclass sup) {
    (void)cls;
    return (*env)->IsAssignableFrom(env, sub, sup);
}
