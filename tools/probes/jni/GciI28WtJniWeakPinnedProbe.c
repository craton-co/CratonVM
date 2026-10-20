/*
 * SPDX-License-Identifier: Apache-2.0
 * Copyright 2024-2026 Craton Software Company
 *
 * Native half of tools/probes/GciI28WtJniWeakPinnedProbe.java (gci i28/wt,
 * 2026-10-15; page
 * docs/known-issues/gc/gci-i27gr-g1-kept-young-region-weak-tables-read-region-liveness-20261015.md):
 * two JNI weak global slots, polled with IsSameObject(w, NULL).
 *
 * Linux:   gcc -O1 -shared -fPIC -I$JDK/include -I$JDK/include/linux
 *          GciI28WtJniWeakPinnedProbe.c -o libgcii28wt.so
 * Windows: cl /LD /O1 /I%JAVA_HOME%\include /I%JAVA_HOME%\include\win32
 *          GciI28WtJniWeakPinnedProbe.c /Fegcii28wt.dll
 */
#include <jni.h>
#include <stddef.h>

static jweak g_slots[2];

/* Slot `slot` (0 or 1) becomes a weak global to `o`; an earlier one is deleted. */
JNIEXPORT void JNICALL
Java_GciI28WtJniWeakPinnedProbe_makeWeak(JNIEnv *env, jclass self, jint slot, jobject o)
{
    (void)self;
    if (slot < 0 || slot > 1) {
        return;
    }
    if (g_slots[slot] != NULL) {
        (*env)->DeleteWeakGlobalRef(env, g_slots[slot]);
    }
    g_slots[slot] = (*env)->NewWeakGlobalRef(env, o);
}

/* The "was it collected?" idiom: IsSameObject(w, NULL), nothing else. A slot
 * never made reads JNI_FALSE. */
JNIEXPORT jboolean JNICALL
Java_GciI28WtJniWeakPinnedProbe_weakCleared(JNIEnv *env, jclass self, jint slot)
{
    (void)self;
    if (slot < 0 || slot > 1 || g_slots[slot] == NULL) {
        return JNI_FALSE;
    }
    return (*env)->IsSameObject(env, g_slots[slot], NULL);
}

JNIEXPORT void JNICALL
Java_GciI28WtJniWeakPinnedProbe_deleteWeak(JNIEnv *env, jclass self, jint slot)
{
    (void)self;
    if (slot < 0 || slot > 1 || g_slots[slot] == NULL) {
        return;
    }
    (*env)->DeleteWeakGlobalRef(env, g_slots[slot]);
    g_slots[slot] = NULL;
}
