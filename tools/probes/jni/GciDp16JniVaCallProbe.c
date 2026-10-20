/*
 * SPDX-License-Identifier: Apache-2.0
 * Copyright 2024-2026 Craton Software Company
 *
 * Native half of tools/probes/GciDp16JniVaCallProbe.java (gci i16/dp,
 * 2026-10-14): loops of va_list up-calls -- variadic Call<Type>Method and an
 * explicit Call<Type>MethodV -- whose checksums the Java side verifies.
 *
 * Windows: cl /LD /O1 /I%JAVA_HOME%\include /I%JAVA_HOME%\include\win32
 *          tools\probes\jni\GciDp16JniVaCallProbe.c /Fegcidp16.dll
 * Linux:   gcc -O1 -shared -fPIC -I$JAVA_HOME/include -I$JAVA_HOME/include/linux
 *          GciDp16JniVaCallProbe.c -o libgcidp16.so
 */
#include <jni.h>
#include <stdarg.h>

/* n x CallStaticIntMethod(cls, add3, i, 1, 2): three ints through the
 * variadic trampoline. */
JNIEXPORT jlong JNICALL
Java_GciDp16JniVaCallProbe_loopAdd3(JNIEnv *env, jclass cls, jint n)
{
    jlong sum = 0;
    jint i;
    jmethodID m = (*env)->GetStaticMethodID(env, cls, "add3", "(III)I");
    if (m == NULL) {
        return -1;
    }
    for (i = 0; i < n; i++) {
        sum += (*env)->CallStaticIntMethod(env, cls, m, i, (jint)1, (jint)2);
        if ((*env)->ExceptionCheck(env)) {
            return -2;
        }
    }
    return sum;
}

/* The V form, called with a va_list this function builds itself. */
static jint call_static_int_v(JNIEnv *env, jclass cls, jmethodID m, ...)
{
    jint r;
    va_list ap;
    va_start(ap, m);
    r = (*env)->CallStaticIntMethodV(env, cls, m, ap);
    va_end(ap);
    return r;
}

/* n x CallStaticIntMethodV(cls, add3, i, 4, 5). */
JNIEXPORT jlong JNICALL
Java_GciDp16JniVaCallProbe_loopV(JNIEnv *env, jclass cls, jint n)
{
    jlong sum = 0;
    jint i;
    jmethodID m = (*env)->GetStaticMethodID(env, cls, "add3", "(III)I");
    if (m == NULL) {
        return -1;
    }
    for (i = 0; i < n; i++) {
        sum += call_static_int_v(env, cls, m, i, (jint)4, (jint)5);
        if ((*env)->ExceptionCheck(env)) {
            return -2;
        }
    }
    return sum;
}

/* n x CallStaticLongMethod(cls, mix, i, 3i, 0.5, 1.5f, o): both register
 * classes and a float the caller promotes to double. */
JNIEXPORT jlong JNICALL
Java_GciDp16JniVaCallProbe_loopMixed(JNIEnv *env, jclass cls, jint n, jobject o)
{
    jlong sum = 0;
    jint i;
    jmethodID m = (*env)->GetStaticMethodID(env, cls, "mix", "(IJDFLjava/lang/Object;)J");
    if (m == NULL) {
        return -1;
    }
    for (i = 0; i < n; i++) {
        sum += (*env)->CallStaticLongMethod(env, cls, m, i, (jlong)i * 3, (jdouble)0.5,
                                            (jfloat)1.5f, o);
        if ((*env)->ExceptionCheck(env)) {
            return -2;
        }
    }
    return sum;
}

/* Called from Java's outer(i): one more variadic up-call, made while the
 * enclosing loopNested up-call's arguments are still in use. */
JNIEXPORT jint JNICALL
Java_GciDp16JniVaCallProbe_inner(JNIEnv *env, jclass cls, jint i)
{
    jmethodID m = (*env)->GetStaticMethodID(env, cls, "add3", "(III)I");
    if (m == NULL) {
        return -1000000;
    }
    return (*env)->CallStaticIntMethod(env, cls, m, i, (jint)10, (jint)20);
}

/* n x CallStaticIntMethod(cls, outer, i): each call nests inner's up-call. */
JNIEXPORT jlong JNICALL
Java_GciDp16JniVaCallProbe_loopNested(JNIEnv *env, jclass cls, jint n)
{
    jlong sum = 0;
    jint i;
    jmethodID m = (*env)->GetStaticMethodID(env, cls, "outer", "(I)I");
    if (m == NULL) {
        return -1;
    }
    for (i = 0; i < n; i++) {
        sum += (*env)->CallStaticIntMethod(env, cls, m, i);
        if ((*env)->ExceptionCheck(env)) {
            return -2;
        }
    }
    return sum;
}

/* n x CallStaticIntMethod(cls, wide18, i, 1, ..., 17): eighteen ints. */
JNIEXPORT jlong JNICALL
Java_GciDp16JniVaCallProbe_loopWide(JNIEnv *env, jclass cls, jint n)
{
    jlong sum = 0;
    jint i;
    jmethodID m = (*env)->GetStaticMethodID(env, cls, "wide18", "(IIIIIIIIIIIIIIIIII)I");
    if (m == NULL) {
        return -1;
    }
    for (i = 0; i < n; i++) {
        sum += (*env)->CallStaticIntMethod(env, cls, m, i, (jint)1, (jint)2, (jint)3, (jint)4,
                                           (jint)5, (jint)6, (jint)7, (jint)8, (jint)9,
                                           (jint)10, (jint)11, (jint)12, (jint)13, (jint)14,
                                           (jint)15, (jint)16, (jint)17);
        if ((*env)->ExceptionCheck(env)) {
            return -2;
        }
    }
    return sum;
}
