/*
 * SPDX-License-Identifier: Apache-2.0
 * Copyright 2024-2026 Craton Software Company
 *
 * Native half of tools/probes/GciJk15JniUpcallArgsProbe.java (gci i15/jk,
 * 2026-10-14): every JNI up-call shape with every parameter type, through the
 * A (jvalue array), V (va_list) and variadic forms; GetStringUTFLengthAsLong
 * against GetStringUTFLength. Every answer is handed back to Java (`see`,
 * `seeJ`, `seeD`), which prints the first round and counts later differences.
 *
 * Windows: cl /LD /O1 /I%JAVA_HOME%\include /I%JAVA_HOME%\include\win32
 *          GciJk15JniUpcallArgsProbe.c /Fegcijk15.dll
 * Linux:   gcc -O1 -shared -fPIC -I$JAVA_HOME/include -I$JAVA_HOME/include/linux
 *          GciJk15JniUpcallArgsProbe.c -o libgcijk15.so
 */
#include <jni.h>
#include <stdarg.h>
#include <stddef.h>

static int ids_ready;
static jclass base_cls; /* global ref: GciJk15JniUpcallArgsProbe$Base */
static jmethodID m_see, m_see_j, m_see_d;
static jmethodID m_i0, m_i1, m_made, m_all, m_sall, m_l7, m_d9, m_s2, m_who;
static jmethodID m_ctor9, m_ctor1;

static int
init_ids(JNIEnv *env, jclass cls)
{
    jclass b;
    if (ids_ready) {
        return 1;
    }
    m_see = (*env)->GetStaticMethodID(env, cls, "see", "(Ljava/lang/String;Ljava/lang/String;)V");
    m_see_j = (*env)->GetStaticMethodID(env, cls, "seeJ", "(Ljava/lang/String;J)V");
    m_see_d = (*env)->GetStaticMethodID(env, cls, "seeD", "(Ljava/lang/String;D)V");
    m_i0 = (*env)->GetMethodID(env, cls, "i0", "()I");
    m_i1 = (*env)->GetMethodID(env, cls, "i1", "(I)I");
    m_made = (*env)->GetMethodID(env, cls, "made", "()Ljava/lang/String;");
    m_all = (*env)->GetMethodID(env, cls, "all",
            "(ZBCSIJFDLjava/lang/Object;[I)Ljava/lang/String;");
    m_sall = (*env)->GetStaticMethodID(env, cls, "sall",
            "(ZBCSIJFDLjava/lang/Object;[IID)Ljava/lang/String;");
    m_l7 = (*env)->GetMethodID(env, cls, "l7", "(JJJJJJJ)J");
    m_d9 = (*env)->GetMethodID(env, cls, "d9", "(DDDDDDDDD)D");
    m_s2 = (*env)->GetStaticMethodID(env, cls, "s2", "(II)I");
    m_ctor9 = (*env)->GetMethodID(env, cls, "<init>", "(ZBCSIJFDLjava/lang/Object;)V");
    m_ctor1 = (*env)->GetMethodID(env, cls, "<init>", "(I)V");
    b = (*env)->FindClass(env, "GciJk15JniUpcallArgsProbe$Base");
    if (b == NULL) {
        return 0;
    }
    m_who = (*env)->GetMethodID(env, b, "who", "(IJDLjava/lang/Object;)Ljava/lang/String;");
    base_cls = (jclass)(*env)->NewGlobalRef(env, b);
    (*env)->DeleteLocalRef(env, b);
    if (!m_see || !m_see_j || !m_see_d || !m_i0 || !m_i1 || !m_made || !m_all || !m_sall
            || !m_l7 || !m_d9 || !m_s2 || !m_ctor9 || !m_ctor1 || !m_who || !base_cls) {
        return 0;
    }
    ids_ready = 1;
    return 1;
}

/* An exception left by an up-call is reported as the answer `exception`. */
static int
pending(JNIEnv *env)
{
    if ((*env)->ExceptionCheck(env)) {
        (*env)->ExceptionDescribe(env);
        (*env)->ExceptionClear(env);
        return 1;
    }
    return 0;
}

static void
see_obj(JNIEnv *env, jclass cls, const char *form, const char *name, jobject got)
{
    char tag[64];
    size_t n = 0;
    const char *p;
    jstring t;
    jstring s = (jstring)got;
    int exc = pending(env);
    for (p = form; *p && n < sizeof(tag) - 1; p++) {
        tag[n++] = *p;
    }
    if (n < sizeof(tag) - 1) {
        tag[n++] = ' ';
    }
    for (p = name; *p && n < sizeof(tag) - 1; p++) {
        tag[n++] = *p;
    }
    tag[n] = 0;
    t = (*env)->NewStringUTF(env, tag);
    if (exc) {
        if (s != NULL) {
            (*env)->DeleteLocalRef(env, s);
        }
        s = (*env)->NewStringUTF(env, "exception");
    }
    (*env)->CallStaticVoidMethod(env, cls, m_see, t, s);
    pending(env);
    (*env)->DeleteLocalRef(env, t);
    if (s != NULL) {
        (*env)->DeleteLocalRef(env, s);
    }
}

static void
see_long(JNIEnv *env, jclass cls, const char *form, const char *name, jlong v)
{
    char tag[64];
    size_t n = 0;
    const char *p;
    jstring t;
    int exc = pending(env);
    for (p = form; *p && n < sizeof(tag) - 1; p++) {
        tag[n++] = *p;
    }
    if (n < sizeof(tag) - 1) {
        tag[n++] = ' ';
    }
    for (p = name; *p && n < sizeof(tag) - 1; p++) {
        tag[n++] = *p;
    }
    tag[n] = 0;
    t = (*env)->NewStringUTF(env, tag);
    if (exc) {
        jstring s = (*env)->NewStringUTF(env, "exception");
        (*env)->CallStaticVoidMethod(env, cls, m_see, t, s);
        (*env)->DeleteLocalRef(env, s);
    } else {
        (*env)->CallStaticVoidMethod(env, cls, m_see_j, t, v);
    }
    pending(env);
    (*env)->DeleteLocalRef(env, t);
}

static void
see_double(JNIEnv *env, jclass cls, const char *form, const char *name, jdouble v)
{
    char tag[64];
    size_t n = 0;
    const char *p;
    jstring t;
    int exc = pending(env);
    for (p = form; *p && n < sizeof(tag) - 1; p++) {
        tag[n++] = *p;
    }
    if (n < sizeof(tag) - 1) {
        tag[n++] = ' ';
    }
    for (p = name; *p && n < sizeof(tag) - 1; p++) {
        tag[n++] = *p;
    }
    tag[n] = 0;
    t = (*env)->NewStringUTF(env, tag);
    if (exc) {
        jstring s = (*env)->NewStringUTF(env, "exception");
        (*env)->CallStaticVoidMethod(env, cls, m_see, t, s);
        (*env)->DeleteLocalRef(env, s);
    } else {
        (*env)->CallStaticVoidMethod(env, cls, m_see_d, t, v);
    }
    pending(env);
    (*env)->DeleteLocalRef(env, t);
}

/* ---- va_list wrappers (the V form) ---- */

static jint
int_v(JNIEnv *env, jobject obj, jmethodID mid, ...)
{
    va_list ap;
    jint r;
    va_start(ap, mid);
    r = (*env)->CallIntMethodV(env, obj, mid, ap);
    va_end(ap);
    return r;
}

static jlong
long_v(JNIEnv *env, jobject obj, jmethodID mid, ...)
{
    va_list ap;
    jlong r;
    va_start(ap, mid);
    r = (*env)->CallLongMethodV(env, obj, mid, ap);
    va_end(ap);
    return r;
}

static jdouble
double_v(JNIEnv *env, jobject obj, jmethodID mid, ...)
{
    va_list ap;
    jdouble r;
    va_start(ap, mid);
    r = (*env)->CallDoubleMethodV(env, obj, mid, ap);
    va_end(ap);
    return r;
}

static jobject
obj_v(JNIEnv *env, jobject obj, jmethodID mid, ...)
{
    va_list ap;
    jobject r;
    va_start(ap, mid);
    r = (*env)->CallObjectMethodV(env, obj, mid, ap);
    va_end(ap);
    return r;
}

static jobject
static_obj_v(JNIEnv *env, jclass cls, jmethodID mid, ...)
{
    va_list ap;
    jobject r;
    va_start(ap, mid);
    r = (*env)->CallStaticObjectMethodV(env, cls, mid, ap);
    va_end(ap);
    return r;
}

static jint
static_int_v(JNIEnv *env, jclass cls, jmethodID mid, ...)
{
    va_list ap;
    jint r;
    va_start(ap, mid);
    r = (*env)->CallStaticIntMethodV(env, cls, mid, ap);
    va_end(ap);
    return r;
}

static jobject
nonvirt_obj_v(JNIEnv *env, jobject obj, jclass cls, jmethodID mid, ...)
{
    va_list ap;
    jobject r;
    va_start(ap, mid);
    r = (*env)->CallNonvirtualObjectMethodV(env, obj, cls, mid, ap);
    va_end(ap);
    return r;
}

static jobject
new_v(JNIEnv *env, jclass cls, jmethodID mid, ...)
{
    va_list ap;
    jobject r;
    va_start(ap, mid);
    r = (*env)->NewObjectV(env, cls, mid, ap);
    va_end(ap);
    return r;
}

/* The argument values every form passes. */
#define ZV JNI_TRUE
#define BV ((jbyte)-7)
#define CV ((jchar)0xD800)
#define SV ((jshort)-300)
#define IV ((jint)123456)
#define JV ((jlong)-9876543210123LL)
#define FV 2.5f
#define DV (-0.125)
#define XV ((jint)-4)
#define YV 6.75
#define WJ ((jlong)10000000000LL)

/* The made() of a constructed object, then the object dropped. */
static void
see_made(JNIEnv *env, jclass cls, const char *form, const char *name, jobject made)
{
    if (made == NULL || pending(env)) {
        see_obj(env, cls, form, name, NULL);
        return;
    }
    see_obj(env, cls, form, name, (*env)->CallObjectMethod(env, made, m_made));
    (*env)->DeleteLocalRef(env, made);
}

static void
round_a(JNIEnv *env, jclass cls, jobject probe, jobject sub, jobject o, jobject a)
{
    const char *f = "A";
    jvalue v[12];
    jvalue lv[7];
    jvalue dv[9];
    jvalue wv[4];
    jvalue one[1];
    jvalue two[2];
    int k;
    v[0].z = ZV;
    v[1].b = BV;
    v[2].c = CV;
    v[3].s = SV;
    v[4].i = IV;
    v[5].j = JV;
    v[6].f = FV;
    v[7].d = DV;
    v[8].l = o;
    v[9].l = a;
    v[10].i = XV;
    v[11].d = YV;
    for (k = 0; k < 7; k++) {
        lv[k].j = (jlong)(k + 1) * 1000000007LL;
    }
    for (k = 0; k < 9; k++) {
        dv[k].d = k + 0.5;
    }
    wv[0].i = 9;
    wv[1].j = WJ;
    wv[2].d = 0.25;
    wv[3].l = o;
    one[0].i = 7;
    two[0].i = 3;
    two[1].i = 4;
    see_long(env, cls, f, "i0", (*env)->CallIntMethodA(env, probe, m_i0, NULL));
    see_long(env, cls, f, "i1", (*env)->CallIntMethodA(env, probe, m_i1, one));
    see_obj(env, cls, f, "made", (*env)->CallObjectMethodA(env, probe, m_made, NULL));
    see_obj(env, cls, f, "all", (*env)->CallObjectMethodA(env, probe, m_all, v));
    see_obj(env, cls, f, "sall", (*env)->CallStaticObjectMethodA(env, cls, m_sall, v));
    see_long(env, cls, f, "l7", (*env)->CallLongMethodA(env, probe, m_l7, lv));
    see_double(env, cls, f, "d9", (*env)->CallDoubleMethodA(env, probe, m_d9, dv));
    see_long(env, cls, f, "s2", (*env)->CallStaticIntMethodA(env, cls, m_s2, two));
    see_obj(env, cls, f, "who-virtual", (*env)->CallObjectMethodA(env, sub, m_who, wv));
    see_obj(env, cls, f, "who-nonvirtual",
            (*env)->CallNonvirtualObjectMethodA(env, sub, base_cls, m_who, wv));
    see_made(env, cls, f, "new9", (*env)->NewObjectA(env, cls, m_ctor9, v));
    {
        jobject n1 = (*env)->NewObjectA(env, cls, m_ctor1, one);
        if (n1 == NULL || pending(env)) {
            see_obj(env, cls, f, "new1", NULL);
        } else {
            see_long(env, cls, f, "new1", (*env)->CallIntMethodA(env, n1, m_i0, NULL));
            (*env)->DeleteLocalRef(env, n1);
        }
    }
}

static void
round_v(JNIEnv *env, jclass cls, jobject probe, jobject sub, jobject o, jobject a)
{
    const char *f = "V";
    see_long(env, cls, f, "i0", int_v(env, probe, m_i0));
    see_long(env, cls, f, "i1", int_v(env, probe, m_i1, (jint)7));
    see_obj(env, cls, f, "made", obj_v(env, probe, m_made));
    see_obj(env, cls, f, "all",
            obj_v(env, probe, m_all, ZV, BV, CV, SV, IV, JV, FV, DV, o, a));
    see_obj(env, cls, f, "sall",
            static_obj_v(env, cls, m_sall, ZV, BV, CV, SV, IV, JV, FV, DV, o, a, XV, YV));
    see_long(env, cls, f, "l7",
            long_v(env, probe, m_l7, (jlong)1000000007LL, (jlong)2000000014LL,
                    (jlong)3000000021LL, (jlong)4000000028LL, (jlong)5000000035LL,
                    (jlong)6000000042LL, (jlong)7000000049LL));
    see_double(env, cls, f, "d9",
            double_v(env, probe, m_d9, 0.5, 1.5, 2.5, 3.5, 4.5, 5.5, 6.5, 7.5, 8.5));
    see_long(env, cls, f, "s2", static_int_v(env, cls, m_s2, (jint)3, (jint)4));
    see_obj(env, cls, f, "who-virtual", obj_v(env, sub, m_who, (jint)9, WJ, 0.25, o));
    see_obj(env, cls, f, "who-nonvirtual",
            nonvirt_obj_v(env, sub, base_cls, m_who, (jint)9, WJ, 0.25, o));
    see_made(env, cls, f, "new9", new_v(env, cls, m_ctor9, ZV, BV, CV, SV, IV, JV, FV, DV, o));
    {
        jobject n1 = new_v(env, cls, m_ctor1, (jint)7);
        if (n1 == NULL || pending(env)) {
            see_obj(env, cls, f, "new1", NULL);
        } else {
            see_long(env, cls, f, "new1", int_v(env, n1, m_i0));
            (*env)->DeleteLocalRef(env, n1);
        }
    }
}

static void
round_va(JNIEnv *env, jclass cls, jobject probe, jobject sub, jobject o, jobject a)
{
    const char *f = "VA";
    see_long(env, cls, f, "i0", (*env)->CallIntMethod(env, probe, m_i0));
    see_long(env, cls, f, "i1", (*env)->CallIntMethod(env, probe, m_i1, (jint)7));
    see_obj(env, cls, f, "made", (*env)->CallObjectMethod(env, probe, m_made));
    see_obj(env, cls, f, "all",
            (*env)->CallObjectMethod(env, probe, m_all, ZV, BV, CV, SV, IV, JV, FV, DV, o, a));
    see_obj(env, cls, f, "sall",
            (*env)->CallStaticObjectMethod(env, cls, m_sall, ZV, BV, CV, SV, IV, JV, FV, DV, o,
                    a, XV, YV));
    see_long(env, cls, f, "l7",
            (*env)->CallLongMethod(env, probe, m_l7, (jlong)1000000007LL, (jlong)2000000014LL,
                    (jlong)3000000021LL, (jlong)4000000028LL, (jlong)5000000035LL,
                    (jlong)6000000042LL, (jlong)7000000049LL));
    see_double(env, cls, f, "d9",
            (*env)->CallDoubleMethod(env, probe, m_d9, 0.5, 1.5, 2.5, 3.5, 4.5, 5.5, 6.5, 7.5,
                    8.5));
    see_long(env, cls, f, "s2", (*env)->CallStaticIntMethod(env, cls, m_s2, (jint)3, (jint)4));
    see_obj(env, cls, f, "who-virtual",
            (*env)->CallObjectMethod(env, sub, m_who, (jint)9, WJ, 0.25, o));
    see_obj(env, cls, f, "who-nonvirtual",
            (*env)->CallNonvirtualObjectMethod(env, sub, base_cls, m_who, (jint)9, WJ, 0.25, o));
    see_made(env, cls, f, "new9",
            (*env)->NewObject(env, cls, m_ctor9, ZV, BV, CV, SV, IV, JV, FV, DV, o));
    {
        jobject n1 = (*env)->NewObject(env, cls, m_ctor1, (jint)7);
        if (n1 == NULL || pending(env)) {
            see_obj(env, cls, f, "new1", NULL);
        } else {
            see_long(env, cls, f, "new1", (*env)->CallIntMethod(env, n1, m_i0));
            (*env)->DeleteLocalRef(env, n1);
        }
    }
}

JNIEXPORT void JNICALL
Java_GciJk15JniUpcallArgsProbe_round(JNIEnv *env, jclass cls, jobject probe, jobject sub,
        jobject o, jintArray a)
{
    if (!init_ids(env, cls)) {
        pending(env);
        return;
    }
    if ((*env)->PushLocalFrame(env, 64) != 0) {
        pending(env);
        return;
    }
    round_a(env, cls, probe, sub, o, a);
    round_v(env, cls, probe, sub, o, a);
    round_va(env, cls, probe, sub, o, a);
    (*env)->PopLocalFrame(env, NULL);
}

JNIEXPORT jlong JNICALL
Java_GciJk15JniUpcallArgsProbe_utfLong(JNIEnv *env, jclass cls, jstring s)
{
    (void)cls;
    return (*env)->GetStringUTFLengthAsLong(env, s);
}

JNIEXPORT jint JNICALL
Java_GciJk15JniUpcallArgsProbe_utfInt(JNIEnv *env, jclass cls, jstring s)
{
    (void)cls;
    return (*env)->GetStringUTFLength(env, s);
}

JNIEXPORT jboolean JNICALL
Java_GciJk15JniUpcallArgsProbe_assignable(JNIEnv *env, jclass cls, jclass a, jclass b)
{
    (void)cls;
    return (*env)->IsAssignableFrom(env, a, b);
}

JNIEXPORT jboolean JNICALL
Java_GciJk15JniUpcallArgsProbe_superIsNull(JNIEnv *env, jclass cls, jclass c)
{
    jclass s;
    (void)cls;
    s = (*env)->GetSuperclass(env, c);
    if (s == NULL) {
        return JNI_TRUE;
    }
    (*env)->DeleteLocalRef(env, s);
    return JNI_FALSE;
}
