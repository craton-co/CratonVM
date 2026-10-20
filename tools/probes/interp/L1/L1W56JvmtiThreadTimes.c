/*
 * Native shim and agent for tools/probes/interp/L1/L1W56JvmtiThreadTimes.java
 * (interpreter round i2 wave 56, lane L1). Loaded twice: as an agent
 * (`-agentpath:...`), whose `Agent_OnLoad` adds
 * `can_get_current_thread_cpu_time` and `can_get_thread_cpu_time`, and with
 * `System.load`, for the native `facts`. Build instructions are in the
 * probe's header; the Rust port
 * tools/probes/interp/L1/rust-ports/L1W56JvmtiThreadTimes prints the same lines.
 */
#include <jni.h>
#include <jvmti.h>
#include <stdarg.h>
#include <stdio.h>
#include <string.h>

static char out[4096];
static jvmtiEnv *agent_env = NULL;
static JavaVM *agent_vm = NULL;
static volatile long sink;

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
    jvmtiCapabilities potential;
    jvmtiCapabilities wanted;
    (void)options;
    (void)reserved;
    if ((*vm)->GetEnv(vm, (void **)&jvmti, JVMTI_VERSION_1_2) != JNI_OK) {
        return 1;
    }
    memset(&potential, 0, sizeof potential);
    (*jvmti)->GetPotentialCapabilities(jvmti, &potential);
    memset(&wanted, 0, sizeof wanted);
    wanted.can_get_current_thread_cpu_time = 1;
    wanted.can_get_thread_cpu_time = 1;
    out[0] = '\0';
    line("onload: potential current=%d thread=%d add=%d", (int)potential.can_get_current_thread_cpu_time,
         (int)potential.can_get_thread_cpu_time, (int)(*jvmti)->AddCapabilities(jvmti, &wanted));
    agent_env = jvmti;
    agent_vm = vm;
    return 0;
}

static void timer_row(const char *label, int err, jvmtiTimerInfo *info)
{
    line("%s: err=%d max=%lld forward=%d backward=%d kind=%d", label, err, (long long)info->max_value,
         (int)info->may_skip_forward, (int)info->may_skip_backward, (int)info->kind);
}

JNIEXPORT jstring JNICALL
Java_L1W56JvmtiThreadTimes_facts(JNIEnv *env, jclass self, jthread other, jthread unstarted, jthread dead,
                                 jobject group, jobject method)
{
    jvmtiEnv *jvmti = agent_env;
    jvmtiEnv *plain = NULL;
    jvmtiTimerInfo info;
    jvmtiCapabilities live;
    jlong before = 0;
    jlong after = 0;
    jlong nanos = -1;
    jint threads = 0;
    jint groups = 0;
    jthread *thread_list = NULL;
    jthreadGroup *group_list = NULL;
    jvmtiThreadInfo thread_info;
    jvmtiThreadGroupInfo group_info;
    jboolean obsolete = 2;
    jstring answer;
    long i;
    int err;
    int err2;
    (void)self;
    if (jvmti == NULL) {
        return (*env)->NewStringUTF(env, "no JVMTI env (load the shim with -agentpath too)\n");
    }
    memset(&info, 0, sizeof info);
    timer_row("current timer", (int)(*jvmti)->GetCurrentThreadCpuTimerInfo(jvmti, &info), &info);
    memset(&info, 0, sizeof info);
    timer_row("thread timer", (int)(*jvmti)->GetThreadCpuTimerInfo(jvmti, &info), &info);
    err = (int)(*jvmti)->GetCurrentThreadCpuTime(jvmti, &before);
    for (i = 0; i < 200000000L; i++) {
        sink += i;
    }
    err2 = (int)(*jvmti)->GetCurrentThreadCpuTime(jvmti, &after);
    line("current time: err=%d err=%d positive=%d advances=%d", err, err2, before > 0 ? 1 : 0,
         after > before ? 1 : 0);
    err = (int)(*jvmti)->GetThreadCpuTime(jvmti, NULL, &nanos);
    line("thread time null: err=%d positive=%d", err, nanos > 0 ? 1 : 0);
    nanos = -1;
    err = (int)(*jvmti)->GetThreadCpuTime(jvmti, other, &nanos);
    line("thread time other: err=%d nonneg=%d", err, nanos >= 0 ? 1 : 0);
    err = (int)(*jvmti)->GetThreadCpuTime(jvmti, unstarted, &nanos);
    line("thread time unstarted: err=%d", err);
    err = (int)(*jvmti)->GetThreadCpuTime(jvmti, dead, &nanos);
    line("thread time dead: err=%d", err);
    err = (int)(*jvmti)->GetThreadCpuTime(jvmti, group, &nanos);
    line("thread time not a thread: err=%d", err);
    line("null out: current=%d thread=%d timer=%d", (int)(*jvmti)->GetCurrentThreadCpuTime(jvmti, NULL),
         (int)(*jvmti)->GetThreadCpuTime(jvmti, NULL, NULL),
         (int)(*jvmti)->GetCurrentThreadCpuTimerInfo(jvmti, NULL));
    if ((*agent_vm)->GetEnv(agent_vm, (void **)&plain, JVMTI_VERSION_1_2) == JNI_OK) {
        memset(&live, 0, sizeof live);
        (*plain)->GetPotentialCapabilities(plain, &live);
        line("live env: potential current=%d thread=%d time=%d timer=%d", (int)live.can_get_current_thread_cpu_time,
             (int)live.can_get_thread_cpu_time, (int)(*plain)->GetCurrentThreadCpuTime(plain, &nanos),
             (int)(*plain)->GetCurrentThreadCpuTimerInfo(plain, &info));
        (*plain)->DisposeEnvironment(plain);
    }
    err = (int)(*jvmti)->GetThreadGroupChildren(jvmti, group, &threads, &thread_list, &groups, &group_list);
    line("group children: err=%d threads=%d groups=%d", err, (int)threads, (int)groups);
    if (err == 0 && threads > 0) {
        memset(&thread_info, 0, sizeof thread_info);
        (*jvmti)->GetThreadInfo(jvmti, thread_list[0], &thread_info);
        line("  thread: %s", thread_info.name == NULL ? "?" : thread_info.name);
        (*jvmti)->Deallocate(jvmti, (unsigned char *)thread_info.name);
    }
    if (err == 0 && groups > 0) {
        memset(&group_info, 0, sizeof group_info);
        (*jvmti)->GetThreadGroupInfo(jvmti, group_list[0], &group_info);
        line("  group: %s", group_info.name == NULL ? "?" : group_info.name);
        (*jvmti)->Deallocate(jvmti, (unsigned char *)group_info.name);
    }
    (*jvmti)->Deallocate(jvmti, (unsigned char *)thread_list);
    (*jvmti)->Deallocate(jvmti, (unsigned char *)group_list);
    err = (int)(*jvmti)->GetThreadGroupChildren(jvmti, other, &threads, &thread_list, &groups, &group_list);
    line("group children of a thread: err=%d", err);
    err = (int)(*jvmti)->IsMethodObsolete(jvmti, (*env)->FromReflectedMethod(env, method), &obsolete);
    line("obsolete: err=%d value=%d", err, (int)obsolete);
    answer = (*env)->NewStringUTF(env, out);
    out[0] = '\0';
    return answer;
}
