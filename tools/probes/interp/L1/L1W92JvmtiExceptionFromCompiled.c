/*
 * Native shim and agent for tools/probes/interp/L1/L1W92JvmtiExceptionFromCompiled.java
 * (interpreter round i4 wave 92, lane L1). Loaded twice: as an agent
 * (`-agentpath:...`), whose `Agent_OnLoad` acquires
 * can_generate_exception_events and can_get_line_numbers and sets an
 * `Exception` callback, and with `System.load`, for the natives `arm` and
 * `rows`. Build instructions are in the probe's header.
 */
#include <jni.h>
#include <jvmti.h>
#include <stdarg.h>
#include <stdio.h>
#include <string.h>

#define MAX_ROWS 32

typedef struct {
    jmethodID method;
    jlocation location;
    jmethodID catch_method;
    jlocation catch_location;
    char text[256];
    long count;
} row_t;

static row_t rows_seen[MAX_ROWS];
static int row_count = 0;
static int overflow = 0;
static char out[16384];
static jvmtiEnv *agent_env = NULL;
static int onload_caps = -1;
static int onload_callbacks = -1;
static jclass probe_class = NULL;

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

/* "name:line" of `location` in `method`, or "none" for a NULL method. */
static void place(jvmtiEnv *jvmti, jmethodID method, jlocation location, char *buf, size_t size)
{
    char *name = NULL;
    jvmtiLineNumberEntry *table = NULL;
    jint entries = 0;
    jint i;
    int best = -1;
    if (method == NULL) {
        snprintf(buf, size, "none");
        return;
    }
    (*jvmti)->GetMethodName(jvmti, method, &name, NULL, NULL);
    if ((*jvmti)->GetLineNumberTable(jvmti, method, &entries, &table) == JVMTI_ERROR_NONE) {
        for (i = 0; i < entries; i++) {
            if (table[i].start_location <= location
                && (best < 0 || table[i].start_location >= table[best].start_location)) {
                best = (int)i;
            }
        }
    }
    if (best >= 0) {
        snprintf(buf, size, "%s:%d", name != NULL ? name : "?", (int)table[best].line_number);
    } else {
        snprintf(buf, size, "%s@%d", name != NULL ? name : "?", (int)location);
    }
    if (table != NULL) {
        (*jvmti)->Deallocate(jvmti, (unsigned char *)table);
    }
    if (name != NULL) {
        (*jvmti)->Deallocate(jvmti, (unsigned char *)name);
    }
}

static void JNICALL
on_exception(jvmtiEnv *jvmti, JNIEnv *env, jthread thread, jmethodID method,
             jlocation location, jobject exception, jmethodID catch_method,
             jlocation catch_location)
{
    jclass declaring = NULL;
    int i;
    (void)thread;
    if (probe_class == NULL) {
        return;
    }
    if ((*jvmti)->GetMethodDeclaringClass(jvmti, method, &declaring) != JVMTI_ERROR_NONE
        || !(*env)->IsSameObject(env, declaring, probe_class)) {
        return;
    }
    for (i = 0; i < row_count; i++) {
        row_t *r = &rows_seen[i];
        if (r->method == method && r->location == location && r->catch_method == catch_method
            && r->catch_location == catch_location) {
            r->count++;
            return;
        }
    }
    if (row_count == MAX_ROWS) {
        overflow++;
        return;
    }
    {
        row_t *r = &rows_seen[row_count++];
        char *sig = NULL;
        char at[128];
        char caught[128];
        jclass exception_class = exception != NULL ? (*env)->GetObjectClass(env, exception) : NULL;
        r->method = method;
        r->location = location;
        r->catch_method = catch_method;
        r->catch_location = catch_location;
        r->count = 1;
        if (exception_class != NULL) {
            (*jvmti)->GetClassSignature(jvmti, exception_class, &sig, NULL);
        }
        place(jvmti, method, location, at, sizeof at);
        place(jvmti, catch_method, catch_location, caught, sizeof caught);
        snprintf(r->text, sizeof r->text, "exception %s at %s catch %s",
                 sig != NULL ? sig : "(no exception object)", at, caught);
        if (sig != NULL) {
            (*jvmti)->Deallocate(jvmti, (unsigned char *)sig);
        }
    }
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
    wanted.can_generate_exception_events = 1;
    wanted.can_get_line_numbers = 1;
    onload_caps = (int)(*jvmti)->AddCapabilities(jvmti, &wanted);
    memset(&callbacks, 0, sizeof callbacks);
    callbacks.Exception = &on_exception;
    onload_callbacks = (int)(*jvmti)->SetEventCallbacks(jvmti, &callbacks, (jint)sizeof callbacks);
    agent_env = jvmti;
    return 0;
}

/* Enable (or disable) `Exception` for the calling thread; the JVMTI error. */
JNIEXPORT jint JNICALL
Java_L1W92JvmtiExceptionFromCompiled_arm(JNIEnv *env, jclass self, jboolean on)
{
    jthread thread = NULL;
    if (agent_env == NULL) {
        return -1;
    }
    if (probe_class == NULL) {
        probe_class = (jclass)(*env)->NewGlobalRef(env, self);
    }
    (*agent_env)->GetCurrentThread(agent_env, &thread);
    return (jint)(*agent_env)->SetEventNotificationMode(
        agent_env, on ? JVMTI_ENABLE : JVMTI_DISABLE, JVMTI_EVENT_EXCEPTION, thread);
}

JNIEXPORT jstring JNICALL
Java_L1W92JvmtiExceptionFromCompiled_rows(JNIEnv *env, jclass self)
{
    int i;
    (void)self;
    out[0] = '\0';
    if (agent_env == NULL) {
        return (*env)->NewStringUTF(env, "no JVMTI env (load the shim with -agentpath too)\n");
    }
    line("onload: caps err=%d callbacks err=%d", onload_caps, onload_callbacks);
    for (i = 0; i < row_count; i++) {
        line("%s x%ld", rows_seen[i].text, rows_seen[i].count);
    }
    if (overflow != 0) {
        line("rows overflowed: %d events not kept", overflow);
    }
    return (*env)->NewStringUTF(env, out);
}
