/*
 * Native shim and agent for tools/probes/interp/L1/L1W102JvmtiPlatformThreadsOnly.java
 * (interpreter round i5 wave 102, lane L1). Loaded twice: as an agent
 * (`-agentpath:...`) and with `System.load`, for the native `rows`. Build
 * instructions are in the probe's header.
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
    (void)options;
    (void)reserved;
    if ((*vm)->GetEnv(vm, (void **)&jvmti, JVMTI_VERSION_1_2) != JNI_OK) {
        return 1;
    }
    agent_env = jvmti;
    return 0;
}

/* Append to `out` the names among `threads` that start with "p-" or "v-",
 * in the order the function returned them, and count the "v-" ones. */
static void
names_of(jvmtiEnv *jvmti, jthread *threads, jint count, char *out, size_t cap,
         int *virtuals)
{
    jint i;
    size_t used = strlen(out);
    for (i = 0; i < count; i++) {
        jvmtiThreadInfo info;
        memset(&info, 0, sizeof info);
        if ((*jvmti)->GetThreadInfo(jvmti, threads[i], &info) != JVMTI_ERROR_NONE
            || info.name == NULL) {
            continue;
        }
        if (strncmp(info.name, "v-", 2) == 0) {
            (*virtuals)++;
        }
        if ((strncmp(info.name, "p-", 2) == 0 || strncmp(info.name, "v-", 2) == 0)
            && used < cap) {
            used += (size_t)snprintf(out + used, cap - used, "%s%s",
                                     used == 0 ? "" : ",", info.name);
        }
        (*jvmti)->Deallocate(jvmti, (unsigned char *)info.name);
    }
}

JNIEXPORT jstring JNICALL
Java_L1W102JvmtiPlatformThreadsOnly_rows(JNIEnv *env, jclass self, jobject platformGroup,
                                         jobject virtualGroup)
{
    jvmtiEnv *jvmti = agent_env;
    static char out[8192];
    char names[2048];
    size_t used = 0;
    jint count = 0, groups = 0, err;
    jthread *threads = NULL;
    jthreadGroup *subgroups = NULL;
    jvmtiStackInfo *stacks = NULL;
    int virtuals;
    (void)self;
    if (jvmti == NULL) {
        return (*env)->NewStringUTF(env, "no JVMTI env (load the shim with -agentpath too)\n");
    }
    out[0] = '\0';

    names[0] = '\0';
    virtuals = 0;
    err = (*jvmti)->GetAllThreads(jvmti, &count, &threads);
    if (err == JVMTI_ERROR_NONE) {
        names_of(jvmti, threads, count, names, sizeof names, &virtuals);
        (*jvmti)->Deallocate(jvmti, (unsigned char *)threads);
    }
    used += (size_t)snprintf(out + used, sizeof out - used,
                             "GetAllThreads err=%d virtual=%d listed=%s\n", (int)err,
                             virtuals, names);

    names[0] = '\0';
    virtuals = 0;
    err = (*jvmti)->GetAllStackTraces(jvmti, 1, &stacks, &count);
    if (err == JVMTI_ERROR_NONE) {
        jthread one[1];
        jint i;
        for (i = 0; i < count; i++) {
            one[0] = stacks[i].thread;
            names_of(jvmti, one, 1, names, sizeof names, &virtuals);
        }
        (*jvmti)->Deallocate(jvmti, (unsigned char *)stacks);
    }
    used += (size_t)snprintf(out + used, sizeof out - used,
                             "GetAllStackTraces err=%d virtual=%d listed=%s\n", (int)err,
                             virtuals, names);

    names[0] = '\0';
    virtuals = 0;
    err = (*jvmti)->GetThreadGroupChildren(jvmti, (jthreadGroup)platformGroup, &count, &threads,
                                          &groups, &subgroups);
    if (err == JVMTI_ERROR_NONE) {
        names_of(jvmti, threads, count, names, sizeof names, &virtuals);
        (*jvmti)->Deallocate(jvmti, (unsigned char *)threads);
        (*jvmti)->Deallocate(jvmti, (unsigned char *)subgroups);
    }
    used += (size_t)snprintf(out + used, sizeof out - used,
                             "GetThreadGroupChildren(main) err=%d virtual=%d listed=%s\n",
                             (int)err, virtuals, names);

    names[0] = '\0';
    virtuals = 0;
    err = (*jvmti)->GetThreadGroupChildren(jvmti, (jthreadGroup)virtualGroup, &count, &threads,
                                          &groups, &subgroups);
    if (err == JVMTI_ERROR_NONE) {
        names_of(jvmti, threads, count, names, sizeof names, &virtuals);
        (*jvmti)->Deallocate(jvmti, (unsigned char *)threads);
        (*jvmti)->Deallocate(jvmti, (unsigned char *)subgroups);
    }
    snprintf(out + used, sizeof out - used,
             "GetThreadGroupChildren(VirtualThreads) err=%d threads=%d virtual=%d\n",
             (int)err, (int)(err == JVMTI_ERROR_NONE ? count : -1), virtuals);
    return (*env)->NewStringUTF(env, out);
}
