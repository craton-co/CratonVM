/*
 * SPDX-License-Identifier: Apache-2.0
 * Copyright 2024-2026 Craton Software Company
 *
 * Native half of BOTH gci i11/cf probes (2026-10-13), one library:
 *
 * - tools/probes/GciCf11StaticArgLoaderProbe.java: a static native of the
 *   nested class GciCf11StaticArgLoaderProbe$F that answers its own class's
 *   static TAG, so the copy of F the call ran on (application loader: 1, the
 *   probe's child-first loader: 2) is visible in the result;
 * - tools/probes/GciCf11JniVirtualSiteProbe.java: instance natives called
 *   through invokeinterface / invokevirtual sites.
 *
 * Windows: cl /LD /O1 /I%JAVA_HOME%\include /I%JAVA_HOME%\include\win32
 *          GciCf11StaticArgLoaderProbe.c /Fegcicf11.dll
 * Linux:   gcc -O1 -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux"
 *          -o libgcicf11.so GciCf11StaticArgLoaderProbe.c
 */
#include <jni.h>

JNIEXPORT jint JNICALL
Java_GciCf11StaticArgLoaderProbe_00024F_id(JNIEnv *env, jclass self, jobject other)
{
    jfieldID tag;
    (void)other;
    tag = (*env)->GetStaticFieldID(env, self, "TAG", "I");
    if (tag == NULL) {
        return -1000;
    }
    return (*env)->GetStaticIntField(env, self, tag);
}

JNIEXPORT jint JNICALL
Java_GciCf11JniVirtualSiteProbe_00024Impl_get(JNIEnv *env, jobject self, jint x)
{
    (void)env;
    (void)self;
    return x + 5;
}

JNIEXPORT jint JNICALL
Java_GciCf11JniVirtualSiteProbe_00024A_val(JNIEnv *env, jobject self, jint x)
{
    (void)env;
    (void)self;
    return x + 1;
}

JNIEXPORT jint JNICALL
Java_GciCf11JniVirtualSiteProbe_00024C_val(JNIEnv *env, jobject self, jint x)
{
    (void)env;
    (void)self;
    return x + 2;
}

JNIEXPORT jint JNICALL
Java_GciCf11JniVirtualSiteProbe_00024A_len(JNIEnv *env, jobject self, jbyteArray b)
{
    (void)self;
    if (b == NULL) {
        return -1;
    }
    return (*env)->GetArrayLength(env, b);
}

JNIEXPORT jint JNICALL
Java_GciCf11JniVirtualSiteProbe_00024A_throwIf(JNIEnv *env, jobject self, jint x)
{
    (void)self;
    if (x == 7) {
        jclass ise = (*env)->FindClass(env, "java/lang/IllegalStateException");
        if (ise != NULL) {
            (*env)->ThrowNew(env, ise, "seven");
        }
        return 0;
    }
    return x * 3;
}
