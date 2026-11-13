/*
 * Native shim and agent for tools/probes/interp/L1/L1W132JvmtiParkBlockerLocal.java
 * (interpreter round i6 wave 132, lane L1; i126-L1). Loaded twice: as an
 * agent (`-agentpath`), whose `Agent_OnLoad` adds `can_suspend` and
 * `can_access_local_variables`, and with `System.load`, for the native
 * `rows(...)`, which suspends a platform thread parked in
 * `LockSupport.park(Object)` and reads that frame's slot 0 (`blocker`, dead
 * at the `Unsafe.park` call by liveness). Build instructions are in the
 * probe's header.
 */
#include <jni.h>
#include <jvmti.h>
#include <stdarg.h>
#include <stdio.h>
#include <string.h>

static char out[8192];
static jvmtiEnv *agent_env = NULL;

JNIEXPORT jint JNICALL
Agent_OnLoad(JavaVM *vm, char *options, void *reserved)
{
    jvmtiEnv *jvmti = NULL;
    jvmtiCapabilities wanted;
    (void)options;
    (void)reserved;
    if ((*vm)->GetEnv(vm, (void **)&jvmti, JVMTI_VERSION_1_2) != JNI_OK) {
        return 1;
    }
    memset(&wanted, 0, sizeof wanted);
    wanted.can_suspend = 1;
    wanted.can_access_local_variables = 1;
    if ((*jvmti)->AddCapabilities(jvmti, &wanted) != JVMTI_ERROR_NONE) {
        return 1;
    }
    agent_env = jvmti;
    return 0;
}

static void line(const char *fmt, ...)
{
    char item[1024];
    va_list ap;
    size_t used = strlen(out);
    va_start(ap, fmt);
    vsnprintf(item, sizeof item, fmt, ap);
    va_end(ap);
    if (used + strlen(item) + 2 < sizeof out) {
        strcat(out, item);
        strcat(out, "\n");
    }
}

/* The depth of the topmost frame of `thread` whose method is `name` `sig`, or -1. */
static jint depth_of(jvmtiEnv *jvmti, jthread thread, const char *want_name, const char *want_sig)
{
    jvmtiFrameInfo frames[32];
    jint n = -1;
    jint k;
    if ((*jvmti)->GetStackTrace(jvmti, thread, 0, 32, frames, &n) != JVMTI_ERROR_NONE) {
        return -1;
    }
    for (k = 0; k < n && k < 32; k++) {
        char *name = NULL;
        char *sig = NULL;
        int same = 0;
        if ((*jvmti)->GetMethodName(jvmti, frames[k].method, &name, &sig, NULL) == JVMTI_ERROR_NONE
            && name != NULL && sig != NULL) {
            same = strcmp(name, want_name) == 0 && strcmp(sig, want_sig) == 0;
        }
        if (name != NULL) {
            (*jvmti)->Deallocate(jvmti, (unsigned char *)name);
        }
        if (sig != NULL) {
            (*jvmti)->Deallocate(jvmti, (unsigned char *)sig);
        }
        if (same) {
            return k;
        }
    }
    return -1;
}

JNIEXPORT jstring JNICALL
Java_L1W132JvmtiParkBlockerLocal_rows(JNIEnv *env, jclass self, jthread parker, jstring label)
{
    jvmtiEnv *jvmti = agent_env;
    const char *what = NULL;
    jint depth;
    jobject oval = NULL;
    jvmtiError err;
    (void)self;
    out[0] = '\0';
    if (jvmti == NULL) {
        return (*env)->NewStringUTF(env, "no JVMTI env (load the shim with -agentpath too)\n");
    }
    what = (*env)->GetStringUTFChars(env, label, NULL);
    err = (*jvmti)->SuspendThread(jvmti, parker);
    line("%s SuspendThread: err=%d", what, (int)err);
    depth = depth_of(jvmti, parker, "park", "(Ljava/lang/Object;)V");
    line("%s frame LockSupport.park(Object): %s", what, depth < 0 ? "missing" : "found");
    if (depth >= 0) {
        err = (*jvmti)->GetLocalObject(jvmti, parker, depth, 0, &oval);
        if (err == JVMTI_ERROR_NONE && oval != NULL) {
            jclass string_class = (*env)->FindClass(env, "java/lang/String");
            if ((*env)->IsInstanceOf(env, oval, string_class)) {
                const char *chars = (*env)->GetStringUTFChars(env, (jstring)oval, NULL);
                line("%s GetLocalObject blocker: err=0 %s", what, chars != NULL ? chars : "?");
                if (chars != NULL) {
                    (*env)->ReleaseStringUTFChars(env, (jstring)oval, chars);
                }
            } else {
                line("%s GetLocalObject blocker: err=0 other", what);
            }
        } else if (err == JVMTI_ERROR_NONE) {
            line("%s GetLocalObject blocker: err=0 null", what);
        } else {
            line("%s GetLocalObject blocker: err=%d", what, (int)err);
        }
    }
    err = (*jvmti)->ResumeThread(jvmti, parker);
    line("%s ResumeThread: err=%d", what, (int)err);
    (*env)->ReleaseStringUTFChars(env, label, what);
    return (*env)->NewStringUTF(env, out);
}
