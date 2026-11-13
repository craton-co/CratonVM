/*
 * Native shim of tools/probes/interp/L1/L1K54JvmtiLateAgentLambdaFrames.java:
 * an agent loaded into the live VM; `stack` lists the calling thread's frames.
 */
#include <jni.h>
#include <jvmti.h>
#include <stdio.h>
#include <string.h>

static jvmtiEnv *agent;

JNIEXPORT jint JNICALL Agent_OnAttach(JavaVM *vm, char *options, void *reserved) {
    if ((*vm)->GetEnv(vm, (void **)&agent, JVMTI_VERSION_1_2) != JNI_OK) {
        return 1;
    }
    return 0;
}

static void mask(const char *in, char *out, size_t cap) {
    size_t at = 0;
    while (*in != '\0' && at + 4 < cap) {
        if (in[0] == '0' && in[1] == 'x') {
            out[at++] = '0';
            out[at++] = 'x';
            out[at++] = '?';
            in += 2;
            while ((*in >= '0' && *in <= '9') || (*in >= 'a' && *in <= 'f')) {
                in++;
            }
        } else {
            out[at++] = *in++;
        }
    }
    out[at] = '\0';
}

JNIEXPORT jstring JNICALL Java_L1K54JvmtiLateAgentLambdaFrames_stack(JNIEnv *env, jclass cls) {
    jvmtiFrameInfo frames[8];
    jint count = 0;
    char out[2048];
    size_t at = 0;
    jvmtiError err;
    int i;
    if (agent == NULL) {
        return (*env)->NewStringUTF(env, "no JVMTI env");
    }
    err = (*agent)->GetStackTrace(agent, NULL, 1, 5, frames, &count);
    at += snprintf(out + at, sizeof out - at, "  err=%d count=%d", (int)err, (int)count);
    for (i = 0; err == JVMTI_ERROR_NONE && i < count; i++) {
        jclass declaring = NULL;
        char *sig = NULL;
        char *name = NULL;
        char masked[512];
        (*agent)->GetMethodDeclaringClass(agent, frames[i].method, &declaring);
        (*agent)->GetClassSignature(agent, declaring, &sig, NULL);
        (*agent)->GetMethodName(agent, frames[i].method, &name, NULL, NULL);
        mask(sig != NULL ? sig : "?", masked, sizeof masked);
        at += snprintf(out + at, sizeof out - at, "\n  %s.%s at %lld", masked, name != NULL ? name : "?",
                       (long long)frames[i].location);
        if (sig != NULL) {
            (*agent)->Deallocate(agent, (unsigned char *)sig);
        }
        if (name != NULL) {
            (*agent)->Deallocate(agent, (unsigned char *)name);
        }
    }
    return (*env)->NewStringUTF(env, out);
}
