/*
 * Native shim and agent for tools/probes/interp/L1/L1W46JvmtiAllStackTraces.java
 * (interpreter round i1 wave 46, lane L1). Loaded twice: as an agent
 * (`-agentpath`), whose `Agent_OnLoad` keeps an env, and with `System.load`,
 * for the native `rows(...)`, which calls GetAllThreads,
 * GetThreadListStackTraces and GetAllStackTraces and answers one line per
 * call, `<row>: <answer>` (`err=<n>` for an error). Build instructions are
 * in the probe's header.
 */
#include <jni.h>
#include <jvmti.h>
#include <stdarg.h>
#include <stdio.h>
#include <string.h>

static char out[8192];
static jvmtiEnv *agent_env = NULL;

/* The thread-state bits this probe compares: java.lang.Thread.State's,
   SUSPENDED and INTERRUPTED. */
#define STATE_MASK (JVMTI_JAVA_LANG_THREAD_STATE_MASK | JVMTI_THREAD_STATE_SUSPENDED \
                    | JVMTI_THREAD_STATE_INTERRUPTED)

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

static jthread known[6];
static const char *known_names[6] = { "sleeper", "spinner", "waiter", "finished", "unstarted", "main" };

static const char *thread_name(JNIEnv *env, jthread thread)
{
    int k;
    if (thread == NULL) {
        return "null";
    }
    for (k = 0; k < 6; k++) {
        if ((*env)->IsSameObject(env, thread, known[k])) {
            return known_names[k];
        }
    }
    return "?";
}

/* One `jvmtiStackInfo` as `<name> st=0x<state> n=<count> <names>`. */
static void info_text(JNIEnv *env, jvmtiEnv *jvmti, const jvmtiStackInfo *info, char *buf,
                      size_t size)
{
    jint k;
    snprintf(buf, size, "%s st=0x%x n=%d", thread_name(env, info->thread),
             (unsigned)(info->state & STATE_MASK), (int)info->frame_count);
    for (k = 0; k < info->frame_count; k++) {
        char *name = NULL;
        strncat(buf, " ", size - strlen(buf) - 1);
        if ((*jvmti)->GetMethodName(jvmti, info->frame_buffer[k].method, &name, NULL, NULL)
                == JVMTI_ERROR_NONE && name != NULL) {
            strncat(buf, name, size - strlen(buf) - 1);
            (*jvmti)->Deallocate(jvmti, (unsigned char *)name);
        } else {
            strncat(buf, "?", size - strlen(buf) - 1);
        }
    }
}

/* `GetThreadListStackTraces(list, max)`: one line per thread. */
static void list_rows(JNIEnv *env, jvmtiEnv *jvmti, const char *row, jint count,
                      const jthread *list, jint max)
{
    jvmtiStackInfo *infos = NULL;
    char buf[400];
    jint k;
    jvmtiError err = (*jvmti)->GetThreadListStackTraces(jvmti, count, list, max, &infos);
    if (err != JVMTI_ERROR_NONE) {
        line("%s: err=%d", row, (int)err);
        return;
    }
    line("%s: err=0", row);
    for (k = 0; k < count; k++) {
        info_text(env, jvmti, &infos[k], buf, sizeof buf);
        line("  %s", buf);
    }
    (*jvmti)->Deallocate(jvmti, (unsigned char *)infos);
}

#define ERR_ROW(row, call) line("%s: err=%d", row, (int)(call))

JNIEXPORT jstring JNICALL
Java_L1W46JvmtiAllStackTraces_rows(JNIEnv *env, jclass self, jthread sleeper, jthread spinner,
                                   jthread waiter, jthread finished, jthread unstarted)
{
    jvmtiEnv *jvmti = agent_env;
    jthread current = NULL;
    jint count = -1;
    jthread *threads = NULL;
    jvmtiStackInfo *infos = NULL;
    jthread list[6];
    char buf[400];
    int seen[6];
    jint k;
    int j;
    jvmtiError err;
    out[0] = '\0';
    if (jvmti == NULL) {
        return (*env)->NewStringUTF(env, "no JVMTI env (load the shim with -agentpath too)\n");
    }
    (*jvmti)->GetCurrentThread(jvmti, &current);
    known[0] = sleeper;
    known[1] = spinner;
    known[2] = waiter;
    known[3] = finished;
    known[4] = unstarted;
    known[5] = current;

    err = (*jvmti)->GetAllThreads(jvmti, &count, &threads);
    memset(seen, 0, sizeof seen);
    if (err == JVMTI_ERROR_NONE) {
        for (k = 0; k < count; k++) {
            for (j = 0; j < 6; j++) {
                if ((*env)->IsSameObject(env, threads[k], known[j])) {
                    seen[j]++;
                }
            }
        }
        (*jvmti)->Deallocate(jvmti, (unsigned char *)threads);
    }
    line("GetAllThreads: err=%d sleeper=%d spinner=%d waiter=%d finished=%d unstarted=%d main=%d",
         (int)err, seen[0], seen[1], seen[2], seen[3], seen[4], seen[5]);
    ERR_ROW("GetAllThreads NULL count_ptr", (*jvmti)->GetAllThreads(jvmti, NULL, &threads));
    ERR_ROW("GetAllThreads NULL threads_ptr", (*jvmti)->GetAllThreads(jvmti, &count, NULL));

    list[0] = sleeper;
    list[1] = waiter;
    list[2] = spinner;
    list_rows(env, jvmti, "GetThreadListStackTraces sleeper waiter spinner max 3", 3, list, 3);
    list[0] = current;
    list_rows(env, jvmti, "GetThreadListStackTraces main max 2", 1, list, 2);
    list[0] = finished;
    list[1] = unstarted;
    list_rows(env, jvmti, "GetThreadListStackTraces finished unstarted max 3", 2, list, 3);
    list[0] = waiter;
    list_rows(env, jvmti, "GetThreadListStackTraces waiter max 0", 1, list, 0);
    list_rows(env, jvmti, "GetThreadListStackTraces count 0", 0, list, 3);
    ERR_ROW("GetThreadListStackTraces count -1",
            (*jvmti)->GetThreadListStackTraces(jvmti, -1, list, 3, &infos));
    ERR_ROW("GetThreadListStackTraces max -1",
            (*jvmti)->GetThreadListStackTraces(jvmti, 1, list, -1, &infos));
    ERR_ROW("GetThreadListStackTraces NULL list",
            (*jvmti)->GetThreadListStackTraces(jvmti, 1, NULL, 3, &infos));
    ERR_ROW("GetThreadListStackTraces NULL stack_info_ptr",
            (*jvmti)->GetThreadListStackTraces(jvmti, 1, list, 3, NULL));
    list[0] = (jthread)self;
    ERR_ROW("GetThreadListStackTraces of a class",
            (*jvmti)->GetThreadListStackTraces(jvmti, 1, list, 3, &infos));

    count = -1;
    err = (*jvmti)->GetAllStackTraces(jvmti, 3, &infos, &count);
    if (err != JVMTI_ERROR_NONE) {
        line("GetAllStackTraces max 3: err=%d", (int)err);
    } else {
        int found = 0;
        line("GetAllStackTraces max 3: err=0 at least four=%d", (int)(count >= 4));
        for (j = 0; j < 6; j++) {
            for (k = 0; k < count; k++) {
                if ((*env)->IsSameObject(env, infos[k].thread, known[j])) {
                    info_text(env, jvmti, &infos[k], buf, sizeof buf);
                    line("  %s", buf);
                    found++;
                }
            }
        }
        line("  known threads listed: %d", found);
        (*jvmti)->Deallocate(jvmti, (unsigned char *)infos);
    }
    ERR_ROW("GetAllStackTraces max -1", (*jvmti)->GetAllStackTraces(jvmti, -1, &infos, &count));
    ERR_ROW("GetAllStackTraces NULL stack_info_ptr",
            (*jvmti)->GetAllStackTraces(jvmti, 3, NULL, &count));
    ERR_ROW("GetAllStackTraces NULL count_ptr", (*jvmti)->GetAllStackTraces(jvmti, 3, &infos, NULL));
    return (*env)->NewStringUTF(env, out);
}
