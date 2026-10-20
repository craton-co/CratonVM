/*
 * SPDX-License-Identifier: Apache-2.0
 * Copyright 2024-2026 Craton Software Company
 *
 * Native half of tools/probes/GciJh14JniClassGlobalRefsProbe.java (gci i14/jh,
 * 2026-10-13): a jclass reached through every handle kind (local, Class
 * parameter, global, weak global, local of a global) driven through every
 * jclass-consuming JNI function; GetObjectRefType of each kind; ThrowNew
 * through each kind; GetStringLength.
 *
 * Windows: cl /LD /O1 /I%JAVA_HOME%\include /I%JAVA_HOME%\include\win32
 *          GciJh14JniClassGlobalRefsProbe.c /Fegcijh14.dll
 * Linux:   gcc -O1 -shared -fPIC -I$JAVA_HOME/include -I$JAVA_HOME/include/linux
 *          GciJh14JniClassGlobalRefsProbe.c -o libgcijh14.so
 */
#include <jni.h>
#include <stddef.h>
#include <stdio.h>

/* Box.reg(), bound only through RegisterNatives. */
static jint JNICALL
box_reg(JNIEnv *env, jclass cls)
{
    (void)env;
    (void)cls;
    return 77;
}

/* 1 (and cleared) when an exception is pending, else 0. */
static int
drain(JNIEnv *env)
{
    if ((*env)->ExceptionCheck(env)) {
        (*env)->ExceptionClear(env);
        return 1;
    }
    return 0;
}

/* The handle of kind `kind` for `param`; *made is 1 for a global, 2 for a
 * weak global, 3 for a local of the global *extra (to delete), else 0. */
static jclass
make_handle(JNIEnv *env, jclass param, jobject instance, jint kind, int *made, jobject *extra)
{
    jclass local;
    *made = 0;
    *extra = NULL;
    local = (*env)->GetObjectClass(env, instance);
    switch (kind) {
    case 0:
        return local;
    case 1:
        return param;
    case 2:
        *made = 1;
        return (jclass)(*env)->NewGlobalRef(env, local);
    case 3:
        *made = 2;
        return (jclass)(*env)->NewWeakGlobalRef(env, local);
    case 4:
        *made = 1;
        return (jclass)(*env)->NewGlobalRef(env, param);
    case 5:
        *made = 2;
        return (jclass)(*env)->NewWeakGlobalRef(env, param);
    default:
        *made = 3;
        *extra = (*env)->NewGlobalRef(env, local);
        if (*extra == NULL) {
            return NULL;
        }
        return (jclass)(*env)->NewLocalRef(env, *extra);
    }
}

static void
drop_handle(JNIEnv *env, jclass h, int made, jobject extra)
{
    if (made == 1) {
        (*env)->DeleteGlobalRef(env, h);
    } else if (made == 2) {
        (*env)->DeleteWeakGlobalRef(env, h);
    } else if (made == 3) {
        (*env)->DeleteLocalRef(env, h);
        (*env)->DeleteGlobalRef(env, extra);
    }
}

JNIEXPORT jstring JNICALL
Java_GciJh14JniClassGlobalRefsProbe_classRefs(JNIEnv *env, jclass self, jclass param,
                                              jobject instance, jclass base, jint kind)
{
    char buf[1024];
    int made;
    jobject extra;
    jclass h;
    jclass local;
    jclass sup;
    jmethodID get;
    jmethodID twice;
    jmethodID ctor;
    jmethodID reg;
    jmethodID back;
    jfieldID v;
    jfieldID s;
    jobject o;
    jobject a;
    jobjectArray arr;
    jobject rm;
    jobject rf;
    jobject mod_h;
    jobject mod_l;
    jvalue args[1];
    JNINativeMethod m;
    int exc = 0;
    int same, super_same, assign_up, assign_down, inst, call, nonvirt, stat, field, sfield;
    int newobj, alloc_ok, alloc_v, array_ok, refl_m, refl_f, refl_call, monitor, module;
    int unreg, regrc, regcall, classof;
    (void)self;
    (*env)->EnsureLocalCapacity(env, 64);
    h = make_handle(env, param, instance, kind, &made, &extra);
    if (h == NULL) {
        drain(env);
        return (*env)->NewStringUTF(env, "no-handle");
    }
    local = (*env)->GetObjectClass(env, instance);

    same = (*env)->IsSameObject(env, h, local) ? 1 : 0;
    sup = (*env)->GetSuperclass(env, h);
    super_same = sup == NULL ? -1 : ((*env)->IsSameObject(env, sup, base) ? 1 : 0);
    exc += drain(env);
    assign_up = (*env)->IsAssignableFrom(env, h, base) ? 1 : 0;
    assign_down = (*env)->IsAssignableFrom(env, base, h) ? 1 : 0;
    inst = (*env)->IsInstanceOf(env, instance, h) ? 1 : 0;
    exc += drain(env);

    get = (*env)->GetMethodID(env, h, "get", "()I");
    exc += drain(env);
    call = -1;
    nonvirt = -1;
    if (get != NULL) {
        call = (*env)->CallIntMethodA(env, instance, get, NULL);
        exc += drain(env);
        nonvirt = (*env)->CallNonvirtualIntMethodA(env, instance, h, get, NULL);
        exc += drain(env);
    }
    twice = (*env)->GetStaticMethodID(env, h, "twice", "(I)I");
    exc += drain(env);
    stat = -1;
    if (twice != NULL) {
        args[0].i = 21;
        stat = (*env)->CallStaticIntMethodA(env, h, twice, args);
        exc += drain(env);
    }
    v = (*env)->GetFieldID(env, h, "v", "I");
    exc += drain(env);
    field = v == NULL ? -1 : (*env)->GetIntField(env, instance, v);
    s = (*env)->GetStaticFieldID(env, h, "S", "I");
    exc += drain(env);
    sfield = -1;
    if (s != NULL) {
        sfield = (*env)->GetStaticIntField(env, h, s);
        (*env)->SetStaticIntField(env, h, s, sfield + 1);
        exc += drain(env);
    }

    ctor = (*env)->GetMethodID(env, h, "<init>", "(I)V");
    exc += drain(env);
    newobj = -1;
    if (ctor != NULL && v != NULL) {
        args[0].i = 5;
        o = (*env)->NewObjectA(env, h, ctor, args);
        exc += drain(env);
        if (o != NULL) {
            newobj = (*env)->GetIntField(env, o, v);
        }
    }
    a = (*env)->AllocObject(env, h);
    exc += drain(env);
    alloc_ok = a != NULL && (*env)->IsInstanceOf(env, a, h) ? 1 : 0;
    alloc_v = (a != NULL && v != NULL) ? (*env)->GetIntField(env, a, v) : -1;

    arr = (*env)->NewObjectArray(env, 2, h, NULL);
    exc += drain(env);
    array_ok = -1;
    if (arr != NULL) {
        (*env)->SetObjectArrayElement(env, arr, 0, instance);
        array_ok = drain(env) ? 0 : (int)(*env)->GetArrayLength(env, arr);
    }

    refl_m = -1;
    refl_f = -1;
    refl_call = -1;
    if (get != NULL) {
        rm = (*env)->ToReflectedMethod(env, h, get, JNI_FALSE);
        exc += drain(env);
        refl_m = rm != NULL ? 1 : 0;
        if (rm != NULL) {
            back = (*env)->FromReflectedMethod(env, rm);
            exc += drain(env);
            if (back != NULL) {
                refl_call = (*env)->CallIntMethodA(env, instance, back, NULL);
                exc += drain(env);
            }
        }
    }
    if (v != NULL) {
        rf = (*env)->ToReflectedField(env, h, v, JNI_FALSE);
        exc += drain(env);
        refl_f = rf != NULL ? 1 : 0;
    }

    monitor = (*env)->MonitorEnter(env, h) == JNI_OK ? 1 : 0;
    if (monitor) {
        monitor = (*env)->MonitorExit(env, h) == JNI_OK ? 1 : 0;
    }
    exc += drain(env);

    mod_h = (*env)->GetModule(env, h);
    mod_l = (*env)->GetModule(env, local);
    exc += drain(env);
    module = (*env)->IsSameObject(env, mod_h, mod_l) ? 1 : 0;

    unreg = (int)(*env)->UnregisterNatives(env, h);
    exc += drain(env);
    m.name = "reg";
    m.signature = "()I";
    m.fnPtr = (void *)box_reg;
    regrc = (int)(*env)->RegisterNatives(env, h, &m, 1);
    exc += drain(env);
    reg = (*env)->GetStaticMethodID(env, h, "reg", "()I");
    exc += drain(env);
    regcall = -1;
    if (reg != NULL) {
        regcall = (*env)->CallStaticIntMethodA(env, h, reg, NULL);
        if (drain(env)) {
            regcall = -2;
        }
    }

    classof = (*env)->IsSameObject(env, (*env)->GetObjectClass(env, h),
                                   (*env)->GetObjectClass(env, local)) ? 1 : 0;
    exc += drain(env);

    snprintf(buf, sizeof buf,
             "same=%d super=%d assign=%d/%d inst=%d call=%d nonvirt=%d static=%d field=%d"
             " sfield=%d newobj=%d alloc=%d/%d array=%d reflect=%d/%d/%d monitor=%d module=%d"
             " register=%d/%d/%d classof=%d exc=%d",
             same, super_same, assign_up, assign_down, inst, call, nonvirt, stat, field,
             sfield, newobj, alloc_ok, alloc_v, array_ok, refl_m, refl_f, refl_call, monitor,
             module, unreg, regrc, regcall, classof, exc);
    drop_handle(env, h, made, extra);
    return (*env)->NewStringUTF(env, buf);
}

JNIEXPORT jint JNICALL
Java_GciJh14JniClassGlobalRefsProbe_refType(JNIEnv *env, jclass self, jclass param,
                                            jobject instance, jint kind)
{
    int made;
    jobject extra;
    jclass h;
    jint t;
    (void)self;
    h = make_handle(env, param, instance, kind, &made, &extra);
    if (h == NULL) {
        drain(env);
        return -1;
    }
    t = (jint)(*env)->GetObjectRefType(env, h);
    drop_handle(env, h, made, extra);
    return t;
}

JNIEXPORT void JNICALL
Java_GciJh14JniClassGlobalRefsProbe_throwVia(JNIEnv *env, jclass self, jclass exc, jint kind)
{
    static const char *const msgs[] = {"via-0", "via-1", "via-2", "via-3", "via-4"};
    jclass found;
    jclass h;
    int made = 0;
    (void)self;
    if (kind < 0 || kind > 4) {
        return;
    }
    switch (kind) {
    case 0:
        h = exc;
        break;
    case 1:
        h = (jclass)(*env)->NewGlobalRef(env, exc);
        made = 1;
        break;
    case 2:
        h = (jclass)(*env)->NewWeakGlobalRef(env, exc);
        made = 2;
        break;
    case 3:
        found = (*env)->FindClass(env, "java/lang/IllegalStateException");
        if (found == NULL) {
            return;
        }
        h = (jclass)(*env)->NewGlobalRef(env, found);
        made = 1;
        break;
    default:
        h = (*env)->FindClass(env, "java/lang/IllegalStateException");
        break;
    }
    if (h == NULL) {
        return;
    }
    (*env)->ThrowNew(env, h, msgs[kind]);
    if (made == 1) {
        (*env)->DeleteGlobalRef(env, h);
    } else if (made == 2) {
        (*env)->DeleteWeakGlobalRef(env, h);
    }
}

JNIEXPORT jint JNICALL
Java_GciJh14JniClassGlobalRefsProbe_strLen(JNIEnv *env, jclass self, jstring s)
{
    (void)self;
    return (jint)(*env)->GetStringLength(env, s);
}

/* NewStringUTF(GetStringUTFChars(s)). */
JNIEXPORT jstring JNICALL
Java_GciJh14JniClassGlobalRefsProbe_utfRoundTrip(JNIEnv *env, jclass self, jstring s)
{
    const char *c;
    jstring back;
    (void)self;
    c = (*env)->GetStringUTFChars(env, s, NULL);
    if (c == NULL) {
        return NULL;
    }
    back = (*env)->NewStringUTF(env, c);
    (*env)->ReleaseStringUTFChars(env, s, c);
    return back;
}

JNIEXPORT jint JNICALL
Java_GciJh14JniClassGlobalRefsProbe_utfLen(JNIEnv *env, jclass self, jstring s)
{
    (void)self;
    return (jint)(*env)->GetStringUTFLength(env, s);
}

/* NewStringUTF of GetStringUTFRegion(s, start, len); NULL with the exception
 * left pending for a bad region, NULL for len outside [0, 16]. */
JNIEXPORT jstring JNICALL
Java_GciJh14JniClassGlobalRefsProbe_utfRegion(JNIEnv *env, jclass self, jstring s, jint start,
                                              jint len)
{
    char buf[64];
    size_t i;
    (void)self;
    if (len < 0 || len > 16) {
        return NULL;
    }
    for (i = 0; i < sizeof buf; i++) {
        buf[i] = 0;
    }
    (*env)->GetStringUTFRegion(env, s, start, len, buf);
    if ((*env)->ExceptionCheck(env)) {
        return NULL;
    }
    return (*env)->NewStringUTF(env, buf);
}
