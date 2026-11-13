/*
 * Native shim and agent for tools/probes/interp/L1/L1W83NativeMethodBind.java
 * (interpreter round i3 wave 83, lane L1). Loaded twice: as an agent
 * (`-agentpath`), whose `Agent_OnLoad` adds
 * `can_generate_native_method_bind_events`, sets the `NativeMethodBind`
 * callback and enables it, and with `System.load`, for the native `rows`.
 *
 * The callback only records the method and whether the address was non-NULL
 * (it may run in the primordial phase, where the method functions answer
 * WRONG_PHASE); `rows` names the recorded methods in the live phase and
 * reports, per method of interest, how many binds it saw, and whether the
 * JDK's own natives (`java/`, `jdk/`, `sun/`) were bound at all. Nothing
 * printed depends on the order of the binds, an address or a count that
 * varies with the JDK's startup.
 */
#include <jni.h>
#include <jvmti.h>
#include <stdio.h>
#include <string.h>

#define MAX_BINDS 16384

static jvmtiEnv *agent_env = NULL;
static jmethodID bound_methods[MAX_BINDS];
static int bound_nonnull[MAX_BINDS];
static int bind_count = 0;
static int bind_overflow = 0;

static void JNICALL
on_native_method_bind(jvmtiEnv *jvmti, JNIEnv *env, jthread thread, jmethodID method, void *address,
                      void **new_address_ptr)
{
    int slot;
    (void)jvmti;
    (void)env;
    (void)thread;
    (void)new_address_ptr;
    slot = __atomic_fetch_add(&bind_count, 1, __ATOMIC_SEQ_CST);
    if (slot >= MAX_BINDS) {
        bind_overflow = 1;
        return;
    }
    bound_methods[slot] = method;
    bound_nonnull[slot] = address != NULL;
}

JNIEXPORT jint JNICALL
Agent_OnLoad(JavaVM *vm, char *options, void *reserved)
{
    jvmtiEnv *jvmti = NULL;
    jvmtiCapabilities wanted;
    jvmtiEventCallbacks callbacks;
    (void)options;
    (void)reserved;
    if ((*vm)->GetEnv(vm, (void **)&jvmti, JVMTI_VERSION_1_2) != JNI_OK) {
        return 1;
    }
    memset(&wanted, 0, sizeof wanted);
    wanted.can_generate_native_method_bind_events = 1;
    if ((*jvmti)->AddCapabilities(jvmti, &wanted) != JVMTI_ERROR_NONE) {
        return 1;
    }
    memset(&callbacks, 0, sizeof callbacks);
    callbacks.NativeMethodBind = on_native_method_bind;
    (*jvmti)->SetEventCallbacks(jvmti, &callbacks, (jint)sizeof callbacks);
    (*jvmti)->SetEventNotificationMode(jvmti, JVMTI_ENABLE, JVMTI_EVENT_NATIVE_METHOD_BIND, NULL);
    agent_env = jvmti;
    return 0;
}

/* "<class signature>.<name><signature>" of `method` into `out`, or "" */
static void
method_key(jmethodID method, char *out, size_t size)
{
    jclass declaring = NULL;
    char *class_sig = NULL;
    char *name = NULL;
    char *sig = NULL;
    out[0] = '\0';
    if ((*agent_env)->GetMethodDeclaringClass(agent_env, method, &declaring) != JVMTI_ERROR_NONE) {
        return;
    }
    if ((*agent_env)->GetClassSignature(agent_env, declaring, &class_sig, NULL) == JVMTI_ERROR_NONE
        && (*agent_env)->GetMethodName(agent_env, method, &name, &sig, NULL) == JVMTI_ERROR_NONE) {
        snprintf(out, size, "%s.%s%s", class_sig, name, sig);
    }
    if (class_sig != NULL) (*agent_env)->Deallocate(agent_env, (unsigned char *)class_sig);
    if (name != NULL) (*agent_env)->Deallocate(agent_env, (unsigned char *)name);
    if (sig != NULL) (*agent_env)->Deallocate(agent_env, (unsigned char *)sig);
}

static const char *const INTEREST[] = {
    "Ljava/lang/Object;.hashCode()I",
    "Ljava/lang/System;.arraycopy(Ljava/lang/Object;ILjava/lang/Object;II)V",
    "Ljava/lang/System;.nanoTime()J",
    "Ljava/lang/Thread;.currentThread()Ljava/lang/Thread;",
    "LL1W83NativeMethodBind;.rows()Ljava/lang/String;",
};
#define INTEREST_COUNT ((int)(sizeof INTEREST / sizeof INTEREST[0]))

JNIEXPORT jstring JNICALL
Java_L1W83NativeMethodBind_rows(JNIEnv *env, jclass self)
{
    char out[4096];
    char key[1024];
    char line[1200];
    int seen[INTEREST_COUNT];
    int nonnull[INTEREST_COUNT];
    int jdk = 0;
    int named = 0;
    int i, j, n;
    (void)self;
    if (agent_env == NULL) {
        return (*env)->NewStringUTF(env, "no JVMTI env (load the shim with -agentpath too)\n");
    }
    memset(seen, 0, sizeof seen);
    memset(nonnull, 0, sizeof nonnull);
    n = __atomic_load_n(&bind_count, __ATOMIC_SEQ_CST);
    if (n > MAX_BINDS) {
        n = MAX_BINDS;
    }
    for (i = 0; i < n; i++) {
        method_key(bound_methods[i], key, sizeof key);
        if (key[0] == '\0') {
            continue;
        }
        named++;
        if (strncmp(key, "Ljava/", 6) == 0 || strncmp(key, "Ljdk/", 5) == 0 || strncmp(key, "Lsun/", 5) == 0) {
            jdk++;
        }
        for (j = 0; j < INTEREST_COUNT; j++) {
            if (strcmp(key, INTEREST[j]) == 0) {
                seen[j]++;
                nonnull[j] |= bound_nonnull[i];
            }
        }
    }
    out[0] = '\0';
    for (j = 0; j < INTEREST_COUNT; j++) {
        snprintf(line, sizeof line, "bind %s: %d address=%s\n", INTEREST[j], seen[j],
                 seen[j] == 0 ? "-" : (nonnull[j] ? "nonnull" : "NULL"));
        strncat(out, line, sizeof out - strlen(out) - 1);
    }
    snprintf(line, sizeof line, "jdk natives bound: %s\nevery bind named: %s\n",
             jdk >= 10 ? "at least 10" : (jdk > 0 ? "fewer than 10" : "none"),
             named == n && !bind_overflow ? "yes" : "no");
    strncat(out, line, sizeof out - strlen(out) - 1);
    return (*env)->NewStringUTF(env, out);
}
