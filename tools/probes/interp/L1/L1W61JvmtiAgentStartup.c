/*
 * Native shim and agent for tools/probes/interp/L1/L1W61JvmtiAgentStartup.java
 * (interpreter round i2 wave 61, lane L1). Loaded twice: as an agent
 * (`-agentpath`), whose `Agent_OnLoad` records the OnLoad-phase rows, and
 * with `System.load`, for the natives. Build instructions are in the
 * probe's header; the Rust port is
 * tools/probes/interp/L1/rust-ports/L1W61JvmtiAgentStartup.
 */
#include <jni.h>
#include <jvmti.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static char onload_out[2048];
static jvmtiEnv *agent_env = NULL;

static void append(char *out, size_t size, const char *fmt, ...)
{
    char item[512];
    va_list ap;
    size_t used = strlen(out);
    va_start(ap, fmt);
    vsnprintf(item, sizeof item, fmt, ap);
    va_end(ap);
    if (used + strlen(item) + 2 < size) {
        strcat(out, item);
        strcat(out, "\n");
    }
}

/* Is `key` among the `count` properties `GetSystemProperties` listed? */
static int listed(char **keys, jint count, const char *key)
{
    jint i;
    for (i = 0; i < count; i++) {
        if (strcmp(keys[i], key) == 0) {
            return 1;
        }
    }
    return 0;
}

static void free_keys(jvmtiEnv *jvmti, char **keys, jint count)
{
    jint i;
    for (i = 0; i < count; i++) {
        (*jvmti)->Deallocate(jvmti, (unsigned char *)keys[i]);
    }
    (*jvmti)->Deallocate(jvmti, (unsigned char *)keys);
}

JNIEXPORT jint JNICALL
Agent_OnLoad(JavaVM *vm, char *options, void *reserved)
{
    jvmtiEnv *jvmti = NULL;
    char *value = NULL;
    char **keys = NULL;
    jint count = 0;
    jvmtiError err;
    jvmtiTimerInfo info;
    (void)options;
    (void)reserved;
    if ((*vm)->GetEnv(vm, (void **)&jvmti, JVMTI_VERSION_1_2) != JNI_OK) {
        return 1;
    }
    agent_env = jvmti;
    onload_out[0] = '\0';
    err = (*jvmti)->GetSystemProperty(jvmti, "java.vm.specification.version", &value);
    append(onload_out, sizeof onload_out, "onload GetSystemProperty java.vm.specification.version: err=%d value=%s",
           (int)err, err == JVMTI_ERROR_NONE ? value : "-");
    if (err == JVMTI_ERROR_NONE) {
        (*jvmti)->Deallocate(jvmti, (unsigned char *)value);
    }
    value = NULL;
    err = (*jvmti)->GetSystemProperty(jvmti, "java.home", &value);
    append(onload_out, sizeof onload_out, "onload GetSystemProperty java.home: err=%d", (int)err);
    if (err == JVMTI_ERROR_NONE) {
        (*jvmti)->Deallocate(jvmti, (unsigned char *)value);
    }
    value = NULL;
    err = (*jvmti)->GetSystemProperty(jvmti, "l1w61.missing", &value);
    append(onload_out, sizeof onload_out, "onload GetSystemProperty l1w61.missing: err=%d", (int)err);
    err = (*jvmti)->GetSystemProperties(jvmti, &count, &keys);
    if (err == JVMTI_ERROR_NONE) {
        append(onload_out, sizeof onload_out,
               "onload GetSystemProperties: err=0 java.home=%d java.vm.version=%d user.dir=%d os.name=%d",
               listed(keys, count, "java.home"), listed(keys, count, "java.vm.version"),
               listed(keys, count, "user.dir"), listed(keys, count, "os.name"));
        free_keys(jvmti, keys, count);
    } else {
        append(onload_out, sizeof onload_out, "onload GetSystemProperties: err=%d", (int)err);
    }
    err = (*jvmti)->SetSystemProperty(jvmti, "java.vm.version", "x");
    append(onload_out, sizeof onload_out, "onload SetSystemProperty java.vm.version: err=%d", (int)err);
    err = (*jvmti)->SetSystemProperty(jvmti, "l1w61.missing", "x");
    append(onload_out, sizeof onload_out, "onload SetSystemProperty l1w61.missing: err=%d", (int)err);
    value = NULL;
    err = (*jvmti)->GetSystemProperty(jvmti, "java.library.path", &value);
    if (err == JVMTI_ERROR_NONE) {
        err = (*jvmti)->SetSystemProperty(jvmti, "java.library.path", value);
        (*jvmti)->Deallocate(jvmti, (unsigned char *)value);
    }
    append(onload_out, sizeof onload_out, "onload SetSystemProperty java.library.path to itself: err=%d", (int)err);
    memset(&info, 0, sizeof info);
    err = (*jvmti)->GetTimerInfo(jvmti, &info);
    append(onload_out, sizeof onload_out, "onload GetTimerInfo: err=%d", (int)err);
    return 0;
}

static jvmtiEnv *env_or_null(void)
{
    return agent_env;
}

#define NO_ENV "no JVMTI env (load the shim with -agentpath too)\n"

JNIEXPORT jstring JNICALL
Java_L1W61JvmtiAgentStartup_onload(JNIEnv *env, jclass self)
{
    (void)self;
    if (env_or_null() == NULL) {
        return (*env)->NewStringUTF(env, NO_ENV);
    }
    return (*env)->NewStringUTF(env, onload_out);
}

JNIEXPORT jstring JNICALL
Java_L1W61JvmtiAgentStartup_getProp(JNIEnv *env, jclass self, jstring key)
{
    jvmtiEnv *jvmti = env_or_null();
    const char *k;
    char *value = NULL;
    char buf[4096];
    jvmtiError err;
    (void)self;
    if (jvmti == NULL) {
        return (*env)->NewStringUTF(env, "!" NO_ENV);
    }
    k = (*env)->GetStringUTFChars(env, key, NULL);
    err = (*jvmti)->GetSystemProperty(jvmti, k, &value);
    (*env)->ReleaseStringUTFChars(env, key, k);
    if (err != JVMTI_ERROR_NONE) {
        snprintf(buf, sizeof buf, "!%d", (int)err);
    } else {
        snprintf(buf, sizeof buf, "=%s", value);
        (*jvmti)->Deallocate(jvmti, (unsigned char *)value);
    }
    return (*env)->NewStringUTF(env, buf);
}

JNIEXPORT jstring JNICALL
Java_L1W61JvmtiAgentStartup_propKeys(JNIEnv *env, jclass self)
{
    jvmtiEnv *jvmti = env_or_null();
    char **keys = NULL;
    jint count = 0;
    jint i;
    char buf[16384];
    jvmtiError err;
    (void)self;
    if (jvmti == NULL) {
        return (*env)->NewStringUTF(env, "!" NO_ENV);
    }
    err = (*jvmti)->GetSystemProperties(jvmti, &count, &keys);
    if (err != JVMTI_ERROR_NONE) {
        snprintf(buf, sizeof buf, "!%d", (int)err);
        return (*env)->NewStringUTF(env, buf);
    }
    buf[0] = '\0';
    for (i = 0; i < count; i++) {
        if (strlen(buf) + strlen(keys[i]) + 2 < sizeof buf) {
            strcat(buf, keys[i]);
            strcat(buf, "\n");
        }
    }
    free_keys(jvmti, keys, count);
    return (*env)->NewStringUTF(env, buf);
}

JNIEXPORT jint JNICALL
Java_L1W61JvmtiAgentStartup_setProp(JNIEnv *env, jclass self, jstring key, jstring value)
{
    jvmtiEnv *jvmti = env_or_null();
    const char *k;
    const char *v;
    jvmtiError err;
    (void)self;
    if (jvmti == NULL) {
        return -1;
    }
    k = (*env)->GetStringUTFChars(env, key, NULL);
    v = (*env)->GetStringUTFChars(env, value, NULL);
    err = (*jvmti)->SetSystemProperty(jvmti, k, v);
    (*env)->ReleaseStringUTFChars(env, value, v);
    (*env)->ReleaseStringUTFChars(env, key, k);
    return (jint)err;
}

JNIEXPORT jstring JNICALL
Java_L1W61JvmtiAgentStartup_nullRows(JNIEnv *env, jclass self, jobject any)
{
    jvmtiEnv *jvmti = env_or_null();
    char out[2048];
    char *value = NULL;
    char **keys = NULL;
    jint count = 0;
    jint hash = 0;
    jlong size = 0;
    (void)self;
    if (jvmti == NULL) {
        return (*env)->NewStringUTF(env, NO_ENV);
    }
    out[0] = '\0';
    append(out, sizeof out, "null GetSystemProperty key: err=%d",
           (int)(*jvmti)->GetSystemProperty(jvmti, NULL, &value));
    append(out, sizeof out, "null GetSystemProperty value_ptr: err=%d",
           (int)(*jvmti)->GetSystemProperty(jvmti, "java.home", NULL));
    append(out, sizeof out, "null GetSystemProperties count_ptr: err=%d",
           (int)(*jvmti)->GetSystemProperties(jvmti, NULL, &keys));
    append(out, sizeof out, "null GetSystemProperties property_ptr: err=%d",
           (int)(*jvmti)->GetSystemProperties(jvmti, &count, NULL));
    append(out, sizeof out, "null SetSystemProperty key: err=%d",
           (int)(*jvmti)->SetSystemProperty(jvmti, NULL, "x"));
    append(out, sizeof out, "null GetObjectHashCode object: err=%d",
           (int)(*jvmti)->GetObjectHashCode(jvmti, NULL, &hash));
    append(out, sizeof out, "null GetObjectHashCode hash_code_ptr: err=%d",
           (int)(*jvmti)->GetObjectHashCode(jvmti, any, NULL));
    append(out, sizeof out, "null GetObjectSize object: err=%d",
           (int)(*jvmti)->GetObjectSize(jvmti, NULL, &size));
    append(out, sizeof out, "null GetObjectSize size_ptr: err=%d",
           (int)(*jvmti)->GetObjectSize(jvmti, any, NULL));
    append(out, sizeof out, "null GetTimerInfo: err=%d", (int)(*jvmti)->GetTimerInfo(jvmti, NULL));
    append(out, sizeof out, "null AddToBootstrapClassLoaderSearch: err=%d",
           (int)(*jvmti)->AddToBootstrapClassLoaderSearch(jvmti, NULL));
    append(out, sizeof out, "null AddToSystemClassLoaderSearch: err=%d",
           (int)(*jvmti)->AddToSystemClassLoaderSearch(jvmti, NULL));
    return (*env)->NewStringUTF(env, out);
}

JNIEXPORT jlong JNICALL
Java_L1W61JvmtiAgentStartup_hash(JNIEnv *env, jclass self, jobject o)
{
    jvmtiEnv *jvmti = env_or_null();
    jint hash = 0;
    jvmtiError err;
    (void)env;
    (void)self;
    if (jvmti == NULL) {
        return -1;
    }
    err = (*jvmti)->GetObjectHashCode(jvmti, o, &hash);
    return ((jlong)err << 32) | ((jlong)hash & 0xffffffffLL);
}

JNIEXPORT jlong JNICALL
Java_L1W61JvmtiAgentStartup_size(JNIEnv *env, jclass self, jobject o)
{
    jvmtiEnv *jvmti = env_or_null();
    jlong size = 0;
    jvmtiError err;
    (void)env;
    (void)self;
    if (jvmti == NULL) {
        return -1;
    }
    err = (*jvmti)->GetObjectSize(jvmti, o, &size);
    return err == JVMTI_ERROR_NONE ? size : -(jlong)err;
}

JNIEXPORT jstring JNICALL
Java_L1W61JvmtiAgentStartup_timer(JNIEnv *env, jclass self)
{
    jvmtiEnv *jvmti = env_or_null();
    jvmtiTimerInfo info;
    char out[256];
    jvmtiError err;
    (void)self;
    if (jvmti == NULL) {
        return (*env)->NewStringUTF(env, NO_ENV);
    }
    memset(&info, 0, sizeof info);
    err = (*jvmti)->GetTimerInfo(jvmti, &info);
    snprintf(out, sizeof out, "timer: err=%d max=%lld forward=%d backward=%d kind=%d\n", (int)err,
             (long long)info.max_value, (int)info.may_skip_forward, (int)info.may_skip_backward,
             (int)info.kind);
    return (*env)->NewStringUTF(env, out);
}

static jint add_search(JNIEnv *env, jstring path, int boot)
{
    jvmtiEnv *jvmti = env_or_null();
    const char *p;
    jvmtiError err;
    if (jvmti == NULL) {
        return -1;
    }
    p = (*env)->GetStringUTFChars(env, path, NULL);
    err = boot ? (*jvmti)->AddToBootstrapClassLoaderSearch(jvmti, p)
               : (*jvmti)->AddToSystemClassLoaderSearch(jvmti, p);
    (*env)->ReleaseStringUTFChars(env, path, p);
    return (jint)err;
}

JNIEXPORT jint JNICALL
Java_L1W61JvmtiAgentStartup_addBoot(JNIEnv *env, jclass self, jstring path)
{
    (void)self;
    return add_search(env, path, 1);
}

JNIEXPORT jint JNICALL
Java_L1W61JvmtiAgentStartup_addSystem(JNIEnv *env, jclass self, jstring path)
{
    (void)self;
    return add_search(env, path, 0);
}
