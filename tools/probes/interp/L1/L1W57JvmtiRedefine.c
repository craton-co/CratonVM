/*
 * Native shim and agent for tools/probes/interp/L1/L1W57JvmtiRedefine.java
 * (interpreter round i2 wave 57, lane L1). Loaded twice: as an agent
 * (`-agentpath:...`), whose `Agent_OnLoad` adds `can_redefine_classes`, and
 * with `System.load`, for the natives. Build instructions are in the probe's
 * header; the Rust port tools/probes/interp/L1/rust-ports/L1W57JvmtiRedefine
 * prints the same lines.
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
    wanted.can_redefine_classes = 1;
    onload_add = (int)(*jvmti)->AddCapabilities(jvmti, &wanted);
    agent_env = jvmti;
    return 0;
}

/* RedefineClasses of one class with `bytes` (NULL: a NULL class file of
 * `count` bytes). */
static int redefine_one(jvmtiEnv *jvmti, JNIEnv *env, jclass k, jbyteArray bytes)
{
    jvmtiClassDefinition def;
    jbyte *elems = NULL;
    int err;
    def.klass = k;
    def.class_byte_count = bytes == NULL ? 3 : (*env)->GetArrayLength(env, bytes);
    if (bytes != NULL) {
        elems = (*env)->GetByteArrayElements(env, bytes, NULL);
    }
    def.class_bytes = (const unsigned char *)elems;
    err = (int)(*jvmti)->RedefineClasses(jvmti, 1, &def);
    if (elems != NULL) {
        (*env)->ReleaseByteArrayElements(env, bytes, elems, JNI_ABORT);
    }
    return err;
}

JNIEXPORT jstring JNICALL
Java_L1W57JvmtiRedefine_redefine(JNIEnv *env, jclass self, jstring label, jclass k, jbyteArray bytes)
{
    jvmtiEnv *jvmti = agent_env;
    const char *text;
    char line[256];
    int err;
    (void)self;
    if (jvmti == NULL) {
        return (*env)->NewStringUTF(env, "no JVMTI env (load the shim with -agentpath too)\n");
    }
    err = redefine_one(jvmti, env, k, bytes);
    text = (*env)->GetStringUTFChars(env, label, NULL);
    snprintf(line, sizeof line, "%s: onload=%d err=%d\n", text, onload_add, err);
    (*env)->ReleaseStringUTFChars(env, label, text);
    return (*env)->NewStringUTF(env, line);
}

/* The refusals, one line. `k` is the target, `bytes` its own class file,
 * `other` another class's. */
JNIEXPORT jstring JNICALL
Java_L1W57JvmtiRedefine_errors(JNIEnv *env, jclass self, jclass k, jbyteArray bytes, jbyteArray other)
{
    jvmtiEnv *jvmti = agent_env;
    jvmtiEnv *live = NULL;
    JavaVM *vm = NULL;
    jvmtiClassDefinition def;
    jvmtiCapabilities potential;
    jbyteArray garbage;
    jclass int_class, array_class;
    int negative, null_defs, null_class, null_bytes, primitive, array, not_a_class, other_class;
    int live_get = -1, live_potential = -1, live_err = -1;
    char out[512];
    (void)self;
    if (jvmti == NULL) {
        return (*env)->NewStringUTF(env, "no JVMTI env (load the shim with -agentpath too)\n");
    }
    memset(&def, 0, sizeof def);
    def.klass = k;
    negative = (int)(*jvmti)->RedefineClasses(jvmti, -1, &def);
    null_defs = (int)(*jvmti)->RedefineClasses(jvmti, 1, NULL);
    null_class = redefine_one(jvmti, env, NULL, bytes);
    null_bytes = redefine_one(jvmti, env, k, NULL);
    int_class = (*env)->GetStaticObjectField(env, (*env)->FindClass(env, "java/lang/Integer"),
                                             (*env)->GetStaticFieldID(env, (*env)->FindClass(env, "java/lang/Integer"),
                                                                      "TYPE", "Ljava/lang/Class;"));
    primitive = redefine_one(jvmti, env, int_class, bytes);
    array_class = (*env)->FindClass(env, "[Ljava/lang/String;");
    array = redefine_one(jvmti, env, array_class, bytes);
    garbage = (*env)->NewByteArray(env, 3);
    not_a_class = redefine_one(jvmti, env, k, garbage);
    other_class = redefine_one(jvmti, env, k, other);
    if ((*env)->GetJavaVM(env, &vm) == 0) {
        live_get = (int)(*vm)->GetEnv(vm, (void **)&live, JVMTI_VERSION_1_2);
        if (live_get == 0) {
            memset(&potential, 0, sizeof potential);
            (*live)->GetPotentialCapabilities(live, &potential);
            live_potential = (int)potential.can_redefine_classes;
            live_err = redefine_one(live, env, k, bytes);
            (*live)->DisposeEnvironment(live);
        }
    }
    snprintf(out, sizeof out,
             "errors: negative=%d null-defs=%d null-class=%d null-bytes=%d primitive=%d array=%d "
             "not-a-class=%d other-class=%d live-env=%d potential=%d err=%d\n",
             negative, null_defs, null_class, null_bytes, primitive, array, not_a_class, other_class, live_get,
             live_potential, live_err);
    return (*env)->NewStringUTF(env, out);
}
