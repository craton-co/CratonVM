// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// gcj j2/jn2: native half of tools/probes/Gcj2Jn2NewStringEdgeProbe.java.
// Windows (MSVC x64 developer prompt, from tools/probes/jni):
//   cl /LD /O1 /I"%JAVA_HOME%\include" /I"%JAVA_HOME%\include\win32" Gcj2Jn2NewStringEdgeProbe.c /Fegcj2jn2ns.dll
// Linux:
//   cc -shared -fPIC -O1 -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//      Gcj2Jn2NewStringEdgeProbe.c -o libgcj2jn2ns.so

#include <jni.h>

JNIEXPORT jstring JNICALL Java_Gcj2Jn2NewStringEdgeProbe_newString(
    JNIEnv *env, jclass self, jint mode, jint len) {
    static const jchar buf[2] = {'a', 'b'};
    (void)self;
    return (*env)->NewString(env, mode == 0 ? NULL : buf, len);
}
