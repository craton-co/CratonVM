/*
 * Native shim and agent for tools/probes/interp/L1/L1W45JvmtiSuspendAndOtherStacks.java
 * (interpreter round i1 wave 45, lane L1). Loaded twice: as an agent
 * (`-agentpath`), whose `Agent_OnLoad` adds `can_suspend`, and with
 * `System.load`, for the native `rows(...)`, which calls the JVMTI
 * suspension, thread-state and stack functions on other threads and answers
 * one line per call, `<row>: <answer>` (`err=<n>` for an error). The rows
 * that need a capability to fail use a second env, obtained in the live
 * phase, which has none. Build instructions are in the probe's header.
 */
#include <jni.h>
#include <jvmti.h>
#include <stdarg.h>
#include <stdio.h>
#include <string.h>

static char out[8192];
static jvmtiEnv *agent_env = NULL;
static jvmtiCapabilities onload_potential;
static int onload_add = -1;

/* The thread-state bits this probe compares: java.lang.Thread.State's,
   SUSPENDED and INTERRUPTED. */
#define STATE_MASK (JVMTI_JAVA_LANG_THREAD_STATE_MASK | JVMTI_THREAD_STATE_SUSPENDED \
                    | JVMTI_THREAD_STATE_INTERRUPTED)

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
    memset(&onload_potential, 0, sizeof onload_potential);
    (*jvmti)->GetPotentialCapabilities(jvmti, &onload_potential);
    memset(&wanted, 0, sizeof wanted);
    wanted.can_suspend = 1;
    onload_add = (int)(*jvmti)->AddCapabilities(jvmti, &wanted);
    agent_env = jvmti;
    return 0;
}

static void line(const char *fmt, ...)
{
    char item[512];
    va_list ap;
    size_t used = strlen(out);
    va_start(ap, fmt);
    vsnprintf(item, sizeof item, fmt, ap);
    va_end(ap);
    if (used + strlen(item) + 2 < sizeof out) {
        strcat(out, item);
        strcat(out, "\n");
    }
}

/* The method name of `method`, or "?". */
static const char *name_of(jvmtiEnv *jvmti, jmethodID method, char *buf, size_t size)
{
    char *name = NULL;
    if ((*jvmti)->GetMethodName(jvmti, method, &name, NULL, NULL) == JVMTI_ERROR_NONE && name != NULL) {
        snprintf(buf, size, "%s", name);
        (*jvmti)->Deallocate(jvmti, (unsigned char *)name);
    } else {
        snprintf(buf, size, "?");
    }
    return buf;
}

/* `GetStackTrace(thread, start, max)` as `err=<n>` or `n=<count> <names>`. */
static void trace_row(jvmtiEnv *jvmti, const char *row, jthread thread, jint start, jint max)
{
    jvmtiFrameInfo frames[32];
    jint n = -1;
    char names[400] = "";
    char buf[128];
    jint k;
    jvmtiError err = (*jvmti)->GetStackTrace(jvmti, thread, start, max, frames, &n);
    if (err != JVMTI_ERROR_NONE) {
        line("%s: err=%d", row, (int)err);
        return;
    }
    for (k = 0; k < n && k < 32; k++) {
        strcat(names, " ");
        strcat(names, name_of(jvmti, frames[k].method, buf, sizeof buf));
    }
    line("%s: n=%d%s", row, (int)n, names);
}

/* `GetThreadState(thread)` as `err=<n>` or `st=0x<masked state>`. */
static void state_row(jvmtiEnv *jvmti, const char *row, jthread thread)
{
    jint state = -1;
    jvmtiError err = (*jvmti)->GetThreadState(jvmti, thread, &state);
    if (err != JVMTI_ERROR_NONE) {
        line("%s: err=%d", row, (int)err);
        return;
    }
    line("%s: st=0x%x", row, (unsigned)(state & STATE_MASK));
}

/* `GetFrameCount(thread)` as `err=<n>` or `err=0 count=<n>`. */
static void count_row(jvmtiEnv *jvmti, const char *row, jthread thread)
{
    jint count = -1;
    jvmtiError err = (*jvmti)->GetFrameCount(jvmti, thread, &count);
    if (err != JVMTI_ERROR_NONE) {
        line("%s: err=%d", row, (int)err);
        return;
    }
    line("%s: err=0 count=%d", row, (int)count);
}

/* `GetFrameLocation(thread, depth)` as `err=<n>` or `err=0 <name>@<location>`. */
static void location_row(jvmtiEnv *jvmti, const char *row, jthread thread, jint depth)
{
    jmethodID method = NULL;
    jlocation location = -2;
    char buf[128];
    jvmtiError err = (*jvmti)->GetFrameLocation(jvmti, thread, depth, &method, &location);
    if (err != JVMTI_ERROR_NONE) {
        line("%s: err=%d", row, (int)err);
        return;
    }
    line("%s: err=0 %s@%d", row, name_of(jvmti, method, buf, sizeof buf), (int)location);
}

/* `Thread.sleep(ms)` through JNI. */
static void sleep_ms(JNIEnv *env, jlong ms)
{
    jclass thread = (*env)->FindClass(env, "java/lang/Thread");
    jmethodID sleep = (*env)->GetStaticMethodID(env, thread, "sleep", "(J)V");
    (*env)->CallStaticVoidMethod(env, thread, sleep, ms);
    if ((*env)->ExceptionCheck(env)) {
        (*env)->ExceptionClear(env);
    }
}

#define ERR_ROW(row, call) line("%s: err=%d", row, (int)(call))

JNIEXPORT jstring JNICALL
Java_L1W45JvmtiSuspendAndOtherStacks_rows(JNIEnv *env, jclass self, jthread sleeper,
                                          jthread spinner, jthread waiter, jthread finished,
                                          jthread unstarted)
{
    JavaVM *vm = NULL;
    jvmtiEnv *jvmti = agent_env;
    jvmtiEnv *bare = NULL;
    jvmtiCapabilities potential;
    jvmtiCapabilities wanted;
    jthread list[3];
    jvmtiError results[3];
    jvmtiError err;
    (void)self;
    out[0] = '\0';
    if (jvmti == NULL || (*env)->GetJavaVM(env, &vm) != JNI_OK
        || (*vm)->GetEnv(vm, (void **)&bare, JVMTI_VERSION_1_2) != JNI_OK) {
        return (*env)->NewStringUTF(env, "no JVMTI env (load the shim with -agentpath too)\n");
    }

    memset(&potential, 0, sizeof potential);
    err = (*bare)->GetPotentialCapabilities(bare, &potential);
    line("live-phase potential: err=%d can_suspend=%d", (int)err, (int)potential.can_suspend);
    line("Agent_OnLoad potential: can_suspend=%d", (int)onload_potential.can_suspend);
    line("Agent_OnLoad AddCapabilities: err=%d", onload_add);
    ERR_ROW("no capability SuspendThread", (*bare)->SuspendThread(bare, sleeper));
    ERR_ROW("no capability ResumeThread", (*bare)->ResumeThread(bare, sleeper));
    list[0] = sleeper;
    ERR_ROW("no capability SuspendThreadList",
            (*bare)->SuspendThreadList(bare, 1, list, results));
    ERR_ROW("no capability ResumeThreadList",
            (*bare)->ResumeThreadList(bare, 1, list, results));
    memset(&wanted, 0, sizeof wanted);
    wanted.can_suspend = 1;
    ERR_ROW("AddCapabilities in the live phase", (*bare)->AddCapabilities(bare, &wanted));

    state_row(bare, "GetThreadState NULL (current)", NULL);
    state_row(bare, "GetThreadState sleeper", sleeper);
    state_row(bare, "GetThreadState spinner", spinner);
    state_row(bare, "GetThreadState waiter", waiter);
    state_row(bare, "GetThreadState finished", finished);
    state_row(bare, "GetThreadState unstarted", unstarted);
    ERR_ROW("GetThreadState NULL state_ptr", (*bare)->GetThreadState(bare, sleeper, NULL));
    ERR_ROW("GetThreadState of a class", (*bare)->GetThreadState(bare, (jthread)self, (jint *)results));

    ERR_ROW("SuspendThread sleeper", (*jvmti)->SuspendThread(jvmti, sleeper));
    ERR_ROW("SuspendThread sleeper again", (*jvmti)->SuspendThread(jvmti, sleeper));
    state_row(jvmti, "GetThreadState sleeper (suspended)", sleeper);
    sleep_ms(env, 300);
    count_row(jvmti, "after 300 ms GetFrameCount sleeper", sleeper);
    trace_row(jvmti, "after 300 ms GetStackTrace sleeper 0 3", sleeper, 0, 3);
    trace_row(jvmti, "after 300 ms GetStackTrace sleeper -1 1", sleeper, -1, 1);
    location_row(jvmti, "after 300 ms GetFrameLocation sleeper 0", sleeper, 0);
    location_row(jvmti, "after 300 ms GetFrameLocation sleeper 1", sleeper, 1);
    state_row(jvmti, "after 300 ms GetThreadState sleeper", sleeper);
    ERR_ROW("ResumeThread sleeper", (*jvmti)->ResumeThread(jvmti, sleeper));
    ERR_ROW("ResumeThread sleeper again", (*jvmti)->ResumeThread(jvmti, sleeper));

    count_row(jvmti, "GetFrameCount spinner (running)", spinner);
    trace_row(jvmti, "GetStackTrace spinner (running) 0 2", spinner, 0, 2);
    state_row(jvmti, "GetThreadState spinner (after the reads)", spinner);
    count_row(jvmti, "GetFrameCount waiter (waiting)", waiter);
    trace_row(jvmti, "GetStackTrace waiter (waiting) 0 3", waiter, 0, 3);
    location_row(jvmti, "GetFrameLocation waiter 0", waiter, 0);

    list[0] = sleeper;
    list[1] = waiter;
    list[2] = sleeper;
    memset(results, 0xff, sizeof results);
    err = (*jvmti)->SuspendThreadList(jvmti, 3, list, results);
    line("SuspendThreadList sleeper waiter sleeper: err=%d results=%d %d %d", (int)err,
         (int)results[0], (int)results[1], (int)results[2]);
    state_row(jvmti, "GetThreadState waiter (suspended)", waiter);
    trace_row(jvmti, "GetStackTrace waiter (suspended) 0 3", waiter, 0, 3);
    memset(results, 0xff, sizeof results);
    err = (*jvmti)->ResumeThreadList(jvmti, 3, list, results);
    line("ResumeThreadList sleeper waiter sleeper: err=%d results=%d %d %d", (int)err,
         (int)results[0], (int)results[1], (int)results[2]);
    state_row(jvmti, "GetThreadState waiter (resumed)", waiter);
    ERR_ROW("SuspendThreadList count 0", (*jvmti)->SuspendThreadList(jvmti, 0, list, results));
    ERR_ROW("SuspendThreadList count -1", (*jvmti)->SuspendThreadList(jvmti, -1, list, results));
    ERR_ROW("SuspendThreadList NULL list", (*jvmti)->SuspendThreadList(jvmti, 1, NULL, results));
    ERR_ROW("SuspendThreadList NULL results", (*jvmti)->SuspendThreadList(jvmti, 1, list, NULL));

    ERR_ROW("SuspendThread finished", (*jvmti)->SuspendThread(jvmti, finished));
    ERR_ROW("ResumeThread finished", (*jvmti)->ResumeThread(jvmti, finished));
    ERR_ROW("SuspendThread unstarted", (*jvmti)->SuspendThread(jvmti, unstarted));
    ERR_ROW("SuspendThread of a class", (*jvmti)->SuspendThread(jvmti, (jthread)self));
    ERR_ROW("ResumeThread spinner (running)", (*jvmti)->ResumeThread(jvmti, spinner));
    count_row(jvmti, "GetFrameCount finished", finished);
    count_row(jvmti, "GetFrameCount unstarted", unstarted);
    return (*env)->NewStringUTF(env, out);
}
