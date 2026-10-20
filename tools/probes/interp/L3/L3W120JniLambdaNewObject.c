// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 120, lane L3: the C half of L3W120JniLambdaNewObject.java
// (JNI GetMethodID / NewObject / NewObjectA / AllocObject of a lambda's class).
// Build (Windows, MSVC x64): cl /LD /I%JAVA_HOME%\include /I%JAVA_HOME%\include\win32 L3W120JniLambdaNewObject.c /Fe:l3w120jni.dll
// Build (Linux): gcc -shared -fPIC -I$JAVA_HOME/include -I$JAVA_HOME/include/linux L3W120JniLambdaNewObject.c -o libl3w120jni.so
#include <jni.h>
#include <stdio.h>
#include <string.h>
static void clearx(JNIEnv *e, char *out, const char *tag) {
  if ((*e)->ExceptionCheck(e)) {
    jthrowable t = (*e)->ExceptionOccurred(e); (*e)->ExceptionClear(e);
    jclass c = (*e)->GetObjectClass(e, t);
    jclass cc = (*e)->FindClass(e, "java/lang/Class");
    jmethodID gn = (*e)->GetMethodID(e, cc, "getName", "()Ljava/lang/String;");
    jstring n = (jstring)(*e)->CallObjectMethod(e, c, gn);
    const char *s = (*e)->GetStringUTFChars(e, n, 0);
    strcat(out, tag); strcat(out, "="); strcat(out, s); strcat(out, " ");
    (*e)->ReleaseStringUTFChars(e, n, s);
  }
}
JNIEXPORT jstring JNICALL Java_L3W120JniLambdaNewObject_probe(JNIEnv *e, jclass k, jclass lc, jstring cap) {
  char out[2048]; out[0] = 0;
  jmethodID init = (*e)->GetMethodID(e, lc, "<init>", "(Ljava/lang/String;I)V");
  clearx(e, out, "GetMethodID(<init>)");
  strcat(out, init ? "init=found " : "init=null ");
  jmethodID get = (*e)->GetMethodID(e, lc, "get", "()Ljava/lang/Object;");
  strcat(out, get ? "get=found " : "get=null ");
  if (init) {
    jobject o = (*e)->NewObject(e, lc, init, cap, (jint)7);
    clearx(e, out, "NewObject");
    if (o) {
      jstring r = (jstring)(*e)->CallObjectMethod(e, o, get);
      clearx(e, out, "get()");
      if (r) { const char *s = (*e)->GetStringUTFChars(e, r, 0); strcat(out, "NewObject.get="); strcat(out, s); strcat(out, " "); (*e)->ReleaseStringUTFChars(e, r, s); }
      strcat(out, (*e)->IsSameObject(e, (*e)->GetObjectClass(e, o), lc) ? "sameClass=true " : "sameClass=false ");
    } else strcat(out, "NewObject=null ");
    jvalue args[2]; args[0].l = cap; args[1].i = 8;
    jobject o2 = (*e)->NewObjectA(e, lc, init, args);
    clearx(e, out, "NewObjectA");
    if (o2) { jstring r = (jstring)(*e)->CallObjectMethod(e, o2, get); if (r) { const char *s = (*e)->GetStringUTFChars(e, r, 0); strcat(out, "NewObjectA.get="); strcat(out, s); strcat(out, " "); (*e)->ReleaseStringUTFChars(e, r, s);} }
  }
  jobject a = (*e)->AllocObject(e, lc);
  clearx(e, out, "AllocObject");
  strcat(out, a ? "alloc=obj" : "alloc=null");
  return (*e)->NewStringUTF(e, out);
}
