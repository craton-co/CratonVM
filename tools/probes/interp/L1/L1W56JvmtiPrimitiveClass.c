/*
 * Native shim and agent for tools/probes/interp/L1/L1W56JvmtiPrimitiveClass.java
 * (interpreter round i2 wave 56, lane L1). Loaded twice: as an agent
 * (`-agentpath:...`), whose `Agent_OnLoad` adds `can_get_source_file_name`,
 * and with `System.load`, for the native `facts`. Build instructions are in
 * the probe's header; the Rust port
 * tools/probes/interp/L1/rust-ports/L1W56JvmtiPrimitiveClass prints the same lines.
 */
#include <jni.h>
#include <jvmti.h>
#include <stdarg.h>
#include <stdio.h>
#include <string.h>

static char out[4096];
static jvmtiEnv *agent_env = NULL;

static void line(const char *fmt, ...)
{
    char item[512];
    va_list ap;
    va_start(ap, fmt);
    vsnprintf(item, sizeof item, fmt, ap);
    va_end(ap);
    if (strlen(out) + strlen(item) + 2 < sizeof out) {
        strcat(out, item);
        strcat(out, "\n");
    }
}

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
    wanted.can_get_source_file_name = 1;
    out[0] = '\0';
    line("onload: add=%d", (int)(*jvmti)->AddCapabilities(jvmti, &wanted));
    agent_env = jvmti;
    return 0;
}

JNIEXPORT jstring JNICALL
Java_L1W56JvmtiPrimitiveClass_facts(JNIEnv *env, jclass self, jclass klass)
{
    jvmtiEnv *jvmti = agent_env;
    char *sig = NULL;
    char *generic = NULL;
    char *source = NULL;
    jint value = -1;
    jint count = -1;
    jint minor = -1;
    jint major = -1;
    jboolean flag = 2;
    jboolean modifiable = 2;
    jobject loader = (jobject)1;
    jmethodID *methods = NULL;
    jfieldID *fields = NULL;
    jclass *interfaces = NULL;
    jstring answer;
    int err;
    (void)self;
    if (jvmti == NULL) {
        return (*env)->NewStringUTF(env, "no JVMTI env (load the shim with -agentpath too)\n");
    }
    err = (int)(*jvmti)->GetClassSignature(jvmti, klass, &sig, &generic);
    line("signature: err=%d sig=%s generic=%s", err, err == 0 && sig != NULL ? sig : "-",
         generic == NULL ? "null" : "set");
    (*jvmti)->Deallocate(jvmti, (unsigned char *)sig);
    (*jvmti)->Deallocate(jvmti, (unsigned char *)generic);
    err = (int)(*jvmti)->GetClassStatus(jvmti, klass, &value);
    line("status: err=%d value=%d", err, (int)value);
    value = -1;
    err = (int)(*jvmti)->GetClassModifiers(jvmti, klass, &value);
    line("modifiers: err=%d value=0x%x", err, (unsigned)value);
    err = (int)(*jvmti)->GetClassMethods(jvmti, klass, &count, &methods);
    line("methods: err=%d count=%d", err, (int)count);
    (*jvmti)->Deallocate(jvmti, (unsigned char *)methods);
    count = -1;
    err = (int)(*jvmti)->GetClassFields(jvmti, klass, &count, &fields);
    line("fields: err=%d count=%d", err, (int)count);
    (*jvmti)->Deallocate(jvmti, (unsigned char *)fields);
    count = -1;
    err = (int)(*jvmti)->GetImplementedInterfaces(jvmti, klass, &count, &interfaces);
    line("interfaces: err=%d count=%d", err, (int)count);
    (*jvmti)->Deallocate(jvmti, (unsigned char *)interfaces);
    err = (int)(*jvmti)->IsInterface(jvmti, klass, &flag);
    line("interface: err=%d value=%d", err, (int)flag);
    flag = 2;
    err = (int)(*jvmti)->IsArrayClass(jvmti, klass, &flag);
    line("array: err=%d value=%d", err, (int)flag);
    err = (int)(*jvmti)->GetClassLoader(jvmti, klass, &loader);
    line("loader: err=%d null=%d", err, loader == NULL ? 1 : 0);
    err = (int)(*jvmti)->GetSourceFileName(jvmti, klass, &source);
    line("source file: err=%d", err);
    (*jvmti)->Deallocate(jvmti, (unsigned char *)source);
    err = (int)(*jvmti)->GetClassVersionNumbers(jvmti, klass, &minor, &major);
    line("version: err=%d", err);
    err = (int)(*jvmti)->IsModifiableClass(jvmti, klass, &modifiable);
    line("modifiable: err=%d value=%d", err, (int)modifiable);
    answer = (*env)->NewStringUTF(env, out);
    out[0] = '\0';
    return answer;
}
