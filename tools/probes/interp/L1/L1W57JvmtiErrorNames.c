/*
 * Native shim and agent for tools/probes/interp/L1/L1W57JvmtiErrorNames.java
 * (interpreter round i2 wave 57, lane L1). Loaded twice: as an agent
 * (`-agentpath:...`), whose `Agent_OnLoad` keeps the JVMTI env, and with
 * `System.load`, for the native `names`. Build instructions are in the
 * probe's header; the Rust port
 * tools/probes/interp/L1/rust-ports/L1W57JvmtiErrorNames prints the same lines.
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

/* One line per code: "<code> <name>" or "<code> err=<e>". */
JNIEXPORT jstring JNICALL
Java_L1W57JvmtiErrorNames_names(JNIEnv *env, jclass self)
{
    jvmtiEnv *jvmti = agent_env;
    static char out[16384];
    size_t used = 0;
    int code;
    (void)self;
    if (jvmti == NULL) {
        return (*env)->NewStringUTF(env, "no JVMTI env (load the shim with -agentpath too)\n");
    }
    out[0] = '\0';
    for (code = 0; code <= 120; code++) {
        char *name = NULL;
        int err = (int)(*jvmti)->GetErrorName(jvmti, (jvmtiError)code, &name);
        if (used < sizeof out) {
            if (err == 0) {
                used += (size_t)snprintf(out + used, sizeof out - used, "%d %s\n", code, name);
            } else {
                used += (size_t)snprintf(out + used, sizeof out - used, "%d err=%d\n", code, err);
            }
        }
        if (name != NULL) {
            (*jvmti)->Deallocate(jvmti, (unsigned char *)name);
        }
    }
    if (used < sizeof out) {
        snprintf(out + used, sizeof out - used, "null out: err=%d\n",
                 (int)(*jvmti)->GetErrorName(jvmti, JVMTI_ERROR_NONE, NULL));
    }
    return (*env)->NewStringUTF(env, out);
}
