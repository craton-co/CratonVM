/*
 * Native shim and agent for tools/probes/interp/L1/L1W46JvmtiReflectiveFrames.java
 * (interpreter round i1 wave 46, lane L1). Loaded twice: as an agent
 * (`-agentpath`), whose `Agent_OnLoad` keeps an env, and with `System.load`,
 * for the native `rows(label)`, which lists the calling thread's stack with
 * GetStackTrace, one frame per line. Build instructions are in the probe's
 * header.
 */
#include <jni.h>
#include <jvmti.h>
#include <stdio.h>
#include <string.h>

static jvmtiEnv *agent_env = NULL;

JNIEXPORT jint JNICALL
Agent_OnLoad(JavaVM *vm, char *options, void *reserved)
{
    jvmtiEnv *jvmti = NULL;
    (void)options;
    (void)reserved;
    if ((*vm)->GetEnv(vm, (void **)&jvmti, JVMTI_VERSION_1_2) != JNI_OK) {
        return 1;
    }
    agent_env = jvmti;
    return 0;
}

/* A frame of HotSpot's own that a stack trace hides: the accessors'
   `@Hidden` `invokeImpl`, and the method-handle frames under it (a class
   name with `LambdaForm$`, `DirectMethodHandle$Holder` or `Invokers$Holder`
   in it). */
static int hidden(const char *class_sig, const char *name)
{
    return strstr(class_sig, "LambdaForm$") != NULL
        || strstr(class_sig, "DirectMethodHandle$Holder") != NULL
        || strstr(class_sig, "Invokers$Holder") != NULL
        || ((strcmp(class_sig, "Ljdk/internal/reflect/DirectMethodHandleAccessor;") == 0
             || strcmp(class_sig, "Ljdk/internal/reflect/DirectConstructorHandleAccessor;") == 0)
            && strcmp(name, "invokeImpl") == 0);
}

JNIEXPORT jstring JNICALL
Java_L1W46JvmtiReflectiveFrames_rows(JNIEnv *env, jclass self, jstring label)
{
    static char out[4096];
    jvmtiEnv *jvmti = agent_env;
    jvmtiFrameInfo frames[12];
    jint n = -1;
    jint k;
    const char *text;
    jvmtiError err;
    (void)self;
    out[0] = '\0';
    if (jvmti == NULL) {
        return (*env)->NewStringUTF(env, "no JVMTI env (load the shim with -agentpath too)\n");
    }
    text = (*env)->GetStringUTFChars(env, label, NULL);
    snprintf(out, sizeof out, "%s:\n", text);
    (*env)->ReleaseStringUTFChars(env, label, text);
    err = (*jvmti)->GetStackTrace(jvmti, NULL, 0, 12, frames, &n);
    if (err != JVMTI_ERROR_NONE) {
        char line[64];
        snprintf(line, sizeof line, "  err=%d\n", (int)err);
        strcat(out, line);
        return (*env)->NewStringUTF(env, out);
    }
    for (k = 0; k < n; k++) {
        char *name = NULL;
        char *sig = NULL;
        jclass declaring = NULL;
        char line[400];
        (*jvmti)->GetMethodName(jvmti, frames[k].method, &name, NULL, NULL);
        (*jvmti)->GetMethodDeclaringClass(jvmti, frames[k].method, &declaring);
        if (declaring != NULL) {
            (*jvmti)->GetClassSignature(jvmti, declaring, &sig, NULL);
        }
        if (!(sig != NULL && name != NULL && hidden(sig, name))) {
            snprintf(line, sizeof line, "  %s.%s@%d\n", sig != NULL ? sig : "?",
                     name != NULL ? name : "?", (int)frames[k].location);
            if (strlen(out) + strlen(line) + 1 < sizeof out) {
                strcat(out, line);
            }
        }
        if (name != NULL) {
            (*jvmti)->Deallocate(jvmti, (unsigned char *)name);
        }
        if (sig != NULL) {
            (*jvmti)->Deallocate(jvmti, (unsigned char *)sig);
        }
    }
    return (*env)->NewStringUTF(env, out);
}
