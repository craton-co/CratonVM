/*
 * Native shim and agent for tools/probes/interp/L1/L1W57JvmtiLocalVariableTable.java
 * (interpreter round i2 wave 57, lane L1). Loaded twice: as an agent
 * (`-agentpath:...`), whose `Agent_OnLoad` adds `can_access_local_variables`
 * (an OnLoad-only capability), and with `System.load`, for the natives.
 * Build instructions are in the probe's header; the Rust port
 * tools/probes/interp/L1/rust-ports/L1W57JvmtiLocalVariableTable prints the
 * same lines.
 */
#include <jni.h>
#include <jvmti.h>
#include <stdio.h>
#include <string.h>

static jvmtiEnv *agent_env = NULL;
static int onload_add = -1;

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
    wanted.can_access_local_variables = 1;
    onload_add = (int)(*jvmti)->AddCapabilities(jvmti, &wanted);
    agent_env = jvmti;
    return 0;
}

static const char *or_null(const char *s)
{
    return s == NULL ? "null" : s;
}

/* "<label>: onload=<a> err=<e> count=<n>", then one line per entry. */
JNIEXPORT jstring JNICALL
Java_L1W57JvmtiLocalVariableTable_table(JNIEnv *env, jclass self, jstring label, jobject m)
{
    jvmtiEnv *jvmti = agent_env;
    jmethodID method;
    jint count = -1;
    jvmtiLocalVariableEntry *table = NULL;
    const char *text;
    char out[4096];
    size_t used;
    int err;
    jint i;
    (void)self;
    if (jvmti == NULL) {
        return (*env)->NewStringUTF(env, "no JVMTI env (load the shim with -agentpath too)\n");
    }
    method = (*env)->FromReflectedMethod(env, m);
    err = (int)(*jvmti)->GetLocalVariableTable(jvmti, method, &count, &table);
    text = (*env)->GetStringUTFChars(env, label, NULL);
    used = (size_t)snprintf(out, sizeof out, "%s: onload=%d err=%d count=%d\n", text, onload_add, err,
                            err == 0 ? (int)count : 0);
    (*env)->ReleaseStringUTFChars(env, label, text);
    if (err == 0) {
        for (i = 0; i < count; i++) {
            jvmtiLocalVariableEntry *e = &table[i];
            if (used < sizeof out) {
                used += (size_t)snprintf(out + used, sizeof out - used,
                                         "  [%d] start=%ld length=%d slot=%d %s %s generic=%s\n", (int)i,
                                         (long)e->start_location, (int)e->length, (int)e->slot,
                                         or_null(e->name), or_null(e->signature),
                                         or_null(e->generic_signature));
            }
            (*jvmti)->Deallocate(jvmti, (unsigned char *)e->name);
            (*jvmti)->Deallocate(jvmti, (unsigned char *)e->signature);
            (*jvmti)->Deallocate(jvmti, (unsigned char *)e->generic_signature);
        }
        (*jvmti)->Deallocate(jvmti, (unsigned char *)table);
    }
    return (*env)->NewStringUTF(env, out);
}

/* The refusals: a NULL method, a NULL count, a NULL table, and an env
 * obtained in the live phase (no `can_access_local_variables`). */
JNIEXPORT jstring JNICALL
Java_L1W57JvmtiLocalVariableTable_errors(JNIEnv *env, jclass self, jobject m)
{
    jvmtiEnv *jvmti = agent_env;
    jvmtiEnv *live = NULL;
    JavaVM *vm = NULL;
    jmethodID method;
    jint count = -1;
    jvmtiLocalVariableEntry *table = NULL;
    int null_method, null_count, null_table, live_err = -1, live_get = -1;
    char out[256];
    (void)self;
    if (jvmti == NULL) {
        return (*env)->NewStringUTF(env, "no JVMTI env (load the shim with -agentpath too)\n");
    }
    method = (*env)->FromReflectedMethod(env, m);
    null_method = (int)(*jvmti)->GetLocalVariableTable(jvmti, NULL, &count, &table);
    null_count = (int)(*jvmti)->GetLocalVariableTable(jvmti, method, NULL, &table);
    null_table = (int)(*jvmti)->GetLocalVariableTable(jvmti, method, &count, NULL);
    if ((*env)->GetJavaVM(env, &vm) == 0) {
        live_get = (int)(*vm)->GetEnv(vm, (void **)&live, JVMTI_VERSION_1_2);
        if (live_get == 0) {
            live_err = (int)(*live)->GetLocalVariableTable(live, method, &count, &table);
            (*live)->DisposeEnvironment(live);
        }
    }
    snprintf(out, sizeof out, "errors: null-method=%d null-count=%d null-table=%d live-env=%d/%d\n", null_method,
             null_count, null_table, live_get, live_err);
    return (*env)->NewStringUTF(env, out);
}
