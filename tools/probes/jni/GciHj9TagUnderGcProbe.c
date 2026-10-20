/*
 * gci wave i9, lane hj: the agent and natives of
 * tools/probes/GciHj9TagUnderGcProbe.java (JVMTI SetTag / GetTag /
 * GetObjectsWithTags from GC-safe callers while collections are requested back
 * to back). Loaded twice: as -agentpath:<lib> (Agent_OnLoad takes the jvmtiEnv
 * with can_tag_objects) and with System.load(<lib>) for the natives.
 *
 *   Windows: cl /LD /O1 /I"%JDK%\include" /I"%JDK%\include\win32" GciHj9TagUnderGcProbe.c /Fegcihj9tag.dll
 *   Linux:   gcc -shared -fPIC -O1 -I$JDK/include -I$JDK/include/linux -o libgcihj9tag.so GciHj9TagUnderGcProbe.c
 */
#include <jni.h>
#include <jvmti.h>
#include <stdlib.h>
#include <string.h>

static jvmtiEnv *jvmti;
static jfieldID value_fid;

/* The agent thread's counters (agentStat): done, rounds, setErr, badGet, badQuery, firstBad. */
static volatile jlong agent_stat[6];
static jobjectArray agent_objs;
static jint agent_rounds;
static jlong agent_base;

JNIEXPORT jint JNICALL Agent_OnLoad(JavaVM *vm, char *options, void *reserved) {
    jvmtiCapabilities caps;
    (void)options;
    (void)reserved;
    if ((*vm)->GetEnv(vm, (void **)&jvmti, JVMTI_VERSION_1_2) != JNI_OK) {
        return JNI_ERR;
    }
    memset(&caps, 0, sizeof(caps));
    caps.can_tag_objects = 1;
    if ((*jvmti)->AddCapabilities(jvmti, &caps) != JVMTI_ERROR_NONE) {
        return JNI_ERR;
    }
    return JNI_OK;
}

JNIEXPORT jint JNICALL Java_GciHj9TagUnderGcProbe_init(JNIEnv *env, jclass cls, jclass node) {
    (void)cls;
    if (jvmti == NULL) {
        return -1;
    }
    value_fid = (*env)->GetFieldID(env, node, "value", "I");
    return value_fid == NULL ? -2 : 0;
}

static jint clear_all(JNIEnv *env, jobjectArray objs) {
    jint n = (*env)->GetArrayLength(env, objs);
    jint errs = 0;
    jint i;
    for (i = 0; i < n; i++) {
        jobject o = (*env)->GetObjectArrayElement(env, objs, i);
        if ((*jvmti)->SetTag(jvmti, o, 0) != JVMTI_ERROR_NONE) {
            errs++;
        }
        (*env)->DeleteLocalRef(env, o);
    }
    return errs;
}

static jint tag_all(JNIEnv *env, jobjectArray objs, jlong base) {
    jint n = (*env)->GetArrayLength(env, objs);
    jint errs = 0;
    jint i;
    for (i = 0; i < n; i++) {
        jobject o = (*env)->GetObjectArrayElement(env, objs, i);
        if ((*jvmti)->SetTag(jvmti, o, base + i) != JVMTI_ERROR_NONE) {
            errs++;
        }
        (*env)->DeleteLocalRef(env, o);
    }
    return errs;
}

static jint check_all(JNIEnv *env, jobjectArray objs, jlong base) {
    jint n = (*env)->GetArrayLength(env, objs);
    jint bad = 0;
    jint i;
    for (i = 0; i < n; i++) {
        jobject o = (*env)->GetObjectArrayElement(env, objs, i);
        jlong tag = -1;
        if ((*jvmti)->GetTag(jvmti, o, &tag) != JVMTI_ERROR_NONE || tag != base + i) {
            bad++;
        }
        (*env)->DeleteLocalRef(env, o);
    }
    return bad;
}

/* bad * 1000000 + found, or -error. */
static jlong query(JNIEnv *env, jlong base, jint n) {
    jlong *wanted;
    jint count = 0;
    jobject *found = NULL;
    jlong *tags = NULL;
    jlong bad = 0;
    jvmtiError err;
    jint i;
    wanted = (jlong *)malloc(sizeof(jlong) * (size_t)(n > 0 ? n : 1));
    if (wanted == NULL) {
        return -1000;
    }
    for (i = 0; i < n; i++) {
        wanted[i] = base + i;
    }
    err = (*jvmti)->GetObjectsWithTags(jvmti, n, wanted, &count, &found, &tags);
    free(wanted);
    if (err != JVMTI_ERROR_NONE) {
        return -(jlong)err;
    }
    for (i = 0; i < count; i++) {
        jlong again = -1;
        jint value = (*env)->GetIntField(env, found[i], value_fid);
        if ((jlong)value != tags[i] - base) {
            bad++;
        } else if ((*jvmti)->GetTag(jvmti, found[i], &again) != JVMTI_ERROR_NONE || again != tags[i]) {
            bad++;
        }
        (*env)->DeleteLocalRef(env, found[i]);
    }
    (*jvmti)->Deallocate(jvmti, (unsigned char *)found);
    (*jvmti)->Deallocate(jvmti, (unsigned char *)tags);
    return bad * 1000000 + count;
}

JNIEXPORT jint JNICALL Java_GciHj9TagUnderGcProbe_clearAll(JNIEnv *env, jclass cls, jobjectArray objs) {
    (void)cls;
    return clear_all(env, objs);
}

JNIEXPORT jint JNICALL Java_GciHj9TagUnderGcProbe_tagAll(JNIEnv *env, jclass cls, jobjectArray objs, jlong base) {
    (void)cls;
    return tag_all(env, objs, base);
}

JNIEXPORT jint JNICALL Java_GciHj9TagUnderGcProbe_checkAll(JNIEnv *env, jclass cls, jobjectArray objs, jlong base) {
    (void)cls;
    return check_all(env, objs, base);
}

JNIEXPORT jlong JNICALL Java_GciHj9TagUnderGcProbe_query(JNIEnv *env, jclass cls, jlong base, jint n) {
    (void)cls;
    return query(env, base, n);
}

static void JNICALL agent_proc(jvmtiEnv *jv, JNIEnv *env, void *arg) {
    jint n;
    jint r;
    (void)jv;
    (void)arg;
    n = (*env)->GetArrayLength(env, agent_objs);
    for (r = 1; r <= agent_rounds; r++) {
        jlong base = agent_base + 10000 * (jlong)r;
        jint e = clear_all(env, agent_objs) + tag_all(env, agent_objs, base);
        jint g = check_all(env, agent_objs, base);
        jlong q = query(env, base, n);
        jlong found = q < 0 ? q : q % 1000000;
        jlong bad = q < 0 ? 0 : q / 1000000;
        agent_stat[2] += e;
        agent_stat[3] += g;
        if (q < 0 || found != n || bad != 0) {
            agent_stat[4]++;
        }
        if (agent_stat[5] == 0 && (e != 0 || g != 0 || q < 0 || found != n || bad != 0)) {
            agent_stat[5] = r;
        }
        agent_stat[1] = r;
    }
    agent_stat[0] = 1;
}

JNIEXPORT jint JNICALL Java_GciHj9TagUnderGcProbe_startAgent(JNIEnv *env, jclass cls, jthread t, jobjectArray objs,
                                                              jint rounds, jlong base) {
    (void)cls;
    agent_objs = (jobjectArray)(*env)->NewGlobalRef(env, objs);
    agent_rounds = rounds;
    agent_base = base;
    return (jint)(*jvmti)->RunAgentThread(jvmti, t, agent_proc, NULL, JVMTI_THREAD_NORM_PRIORITY);
}

JNIEXPORT jlong JNICALL Java_GciHj9TagUnderGcProbe_agentStat(JNIEnv *env, jclass cls, jint which) {
    (void)env;
    (void)cls;
    return which >= 0 && which < 6 ? agent_stat[which] : -1;
}
