/*
 * SPDX-License-Identifier: Apache-2.0
 * Copyright 2024-2026 Craton Software Company
 *
 * Native half of tools/probes/GciCr10StaticFallbackProbe.java (gci i10/cr,
 * 2026-10-13): a dlsym-bound static native declared in the nested class
 * GciCr10StaticFallbackProbe$P, called through its subclass $C.
 *
 * Windows: cl /LD /O1 /I%JAVA_HOME%\include /I%JAVA_HOME%\include\win32
 *          GciCr10StaticFallbackProbe.c /Fegcicr10staticfallback.dll
 * Linux:   gcc -O1 -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux"
 *          -o libgcicr10staticfallback.so GciCr10StaticFallbackProbe.c
 */
#include <jni.h>

JNIEXPORT jint JNICALL
Java_GciCr10StaticFallbackProbe_00024P_pn(JNIEnv *env, jclass self, jint x)
{
    (void)env;
    (void)self;
    return x + 10;
}
