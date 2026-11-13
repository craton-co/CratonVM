/*
 * Native shim and agent for tools/probes/interp/L1/L1W103JvmtiLocalScopeEnd.java
 * (interpreter round i5 wave 103, lane L1). Loaded twice: as an agent
 * (`-agentpath:...`), whose `Agent_OnLoad` adds `can_access_local_variables`,
 * and with `System.load`, for the native `rows`. Build instructions are in
 * the probe's header.
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
    jvmtiCapabilities caps;
    (void)options;
    (void)reserved;
    if ((*vm)->GetEnv(vm, (void **)&jvmti, JVMTI_VERSION_1_2) != JNI_OK) {
        return 1;
    }
    memset(&caps, 0, sizeof caps);
    caps.can_access_local_variables = 1;
    if ((*jvmti)->AddCapabilities(jvmti, &caps) != JVMTI_ERROR_NONE) {
        return 1;
    }
    agent_env = jvmti;
    return 0;
}

/*
 * Called from `m` right after the block that declares `x`: `m`'s frame is
 * at depth 1, at the bci that ends `x`'s scope.
 */
JNIEXPORT jstring JNICALL
Java_L1W103JvmtiLocalScopeEnd_rows(JNIEnv *env, jclass self)
{
    jvmtiEnv *jvmti = agent_env;
    static char out[4096];
    size_t used = 0;
    jmethodID method = NULL;
    jlocation location = -1;
    jvmtiLocalVariableEntry *table = NULL;
    jint entries = 0;
    jint slot = -1;
    int at_end = 0;
    char *name = NULL;
    jint i;
    jint ival = -1;
    jobject oval = NULL;
    jfloat fval = 0;
    int err;
    (void)self;
    if (jvmti == NULL) {
        return (*env)->NewStringUTF(env, "no JVMTI env (load the shim with -agentpath too)\n");
    }
    out[0] = '\0';
    err = (int)(*jvmti)->GetFrameLocation(jvmti, NULL, 1, &method, &location);
    if (err != 0) {
        snprintf(out, sizeof out, "GetFrameLocation 1: err=%d\n", err);
        return (*env)->NewStringUTF(env, out);
    }
    (*jvmti)->GetMethodName(jvmti, method, &name, NULL, NULL);
    /* (entry_count_ptr, table_ptr): the w103 run crashed with the two
     * swapped, which wrote the table pointer into a jint. */
    err = (int)(*jvmti)->GetLocalVariableTable(jvmti, method, &entries, &table);
    if (err != 0) {
        snprintf(out, sizeof out, "GetLocalVariableTable: err=%d\n", err);
        return (*env)->NewStringUTF(env, out);
    }
    for (i = 0; i < entries; i++) {
        if (strcmp(table[i].name, "x") == 0) {
            slot = table[i].slot;
            at_end = table[i].start_location + table[i].length == location;
        }
    }
    used += (size_t)snprintf(out + used, sizeof out - used,
                             "frame: %s at the end of x's scope: %d\n",
                             name != NULL ? name : "?", at_end);
    if (slot < 0) {
        used += (size_t)snprintf(out + used, sizeof out - used, "no entry for x\n");
        return (*env)->NewStringUTF(env, out);
    }
    err = (int)(*jvmti)->GetLocalInt(jvmti, NULL, 1, slot, &ival);
    if (err == 0) {
        used += (size_t)snprintf(out + used, sizeof out - used,
                                 "GetLocalInt x: err=0 value=%d\n", (int)ival);
    } else {
        used += (size_t)snprintf(out + used, sizeof out - used, "GetLocalInt x: err=%d\n", err);
    }
    err = (int)(*jvmti)->GetLocalObject(jvmti, NULL, 1, slot, &oval);
    used += (size_t)snprintf(out + used, sizeof out - used, "GetLocalObject x: err=%d\n", err);
    err = (int)(*jvmti)->GetLocalFloat(jvmti, NULL, 1, slot, &fval);
    used += (size_t)snprintf(out + used, sizeof out - used, "GetLocalFloat x: err=%d\n", err);
    err = (int)(*jvmti)->SetLocalInt(jvmti, NULL, 1, slot, 9);
    used += (size_t)snprintf(out + used, sizeof out - used, "SetLocalInt x=9: err=%d\n", err);
    ival = -1;
    err = (int)(*jvmti)->GetLocalInt(jvmti, NULL, 1, slot, &ival);
    if (err == 0) {
        used += (size_t)snprintf(out + used, sizeof out - used,
                                 "GetLocalInt x after the write: err=0 value=%d\n", (int)ival);
    } else {
        used += (size_t)snprintf(out + used, sizeof out - used,
                                 "GetLocalInt x after the write: err=%d\n", err);
    }
    (void)used;
    return (*env)->NewStringUTF(env, out);
}
