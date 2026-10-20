/*
 * Native shim and agent for tools/probes/interp/L1/L1W87TagsAcrossGc.java
 * (interpreter round i4 wave 87, lane L1). Loaded twice: as an agent
 * (-agentpath; Agent_OnLoad adds can_tag_objects and
 * can_generate_object_free_events and enables ObjectFree) and with
 * System.load for the natives.
 */
#include <jni.h>
#include <jvmti.h>
#include <stdio.h>
#include <string.h>

static jvmtiEnv *agent_env = NULL;
static volatile jint freed = 0;
static volatile jlong freed_sum = 0;

static void JNICALL on_object_free(jvmtiEnv *jvmti, jlong tag)
{
    (void)jvmti;
    freed++;
    freed_sum += tag;
}

JNIEXPORT jint JNICALL Agent_OnLoad(JavaVM *vm, char *options, void *reserved)
{
    jvmtiCapabilities caps;
    jvmtiEventCallbacks callbacks;
    (void)options;
    (void)reserved;
    if ((*vm)->GetEnv(vm, (void **)&agent_env, JVMTI_VERSION_1_2) != JNI_OK) {
        return 1;
    }
    memset(&caps, 0, sizeof caps);
    caps.can_tag_objects = 1;
    caps.can_generate_object_free_events = 1;
    if ((*agent_env)->AddCapabilities(agent_env, &caps) != JVMTI_ERROR_NONE) {
        return 2;
    }
    memset(&callbacks, 0, sizeof callbacks);
    callbacks.ObjectFree = on_object_free;
    (*agent_env)->SetEventCallbacks(agent_env, &callbacks, (jint)sizeof callbacks);
    (*agent_env)->SetEventNotificationMode(agent_env, JVMTI_ENABLE, JVMTI_EVENT_OBJECT_FREE, NULL);
    return 0;
}

/* Tag objs[i] with base + i; answers the first error, or 0. */
JNIEXPORT jint JNICALL Java_L1W87TagsAcrossGc_tagAll(JNIEnv *env, jclass cls, jobjectArray objs,
                                                    jlong base)
{
    jint n = (*env)->GetArrayLength(env, objs);
    jint i;
    (void)cls;
    for (i = 0; i < n; i++) {
        jobject o = (*env)->GetObjectArrayElement(env, objs, i);
        jvmtiError err = (*agent_env)->SetTag(agent_env, o, base + i);
        (*env)->DeleteLocalRef(env, o);
        if (err != JVMTI_ERROR_NONE) {
            return (jint)err;
        }
    }
    return 0;
}

/* How many objs[i] do not carry base + i (a null element is skipped). */
JNIEXPORT jint JNICALL Java_L1W87TagsAcrossGc_mismatches(JNIEnv *env, jclass cls, jobjectArray objs,
                                                        jlong base)
{
    jint n = (*env)->GetArrayLength(env, objs);
    jint i;
    jint bad = 0;
    (void)cls;
    for (i = 0; i < n; i++) {
        jlong tag = -1;
        jobject o = (*env)->GetObjectArrayElement(env, objs, i);
        if (o == NULL) {
            continue;
        }
        if ((*agent_env)->GetTag(agent_env, o, &tag) != JVMTI_ERROR_NONE || tag != base + i) {
            bad++;
        }
        (*env)->DeleteLocalRef(env, o);
    }
    return bad;
}

/* GetObjectsWithTags for base .. base + n - 1: "count sumOfTags sameObject"
 * where sameObject counts results whose object is objs[tag - base]. */
JNIEXPORT jstring JNICALL Java_L1W87TagsAcrossGc_withTags(JNIEnv *env, jclass cls,
                                                          jobjectArray objs, jlong base, jint n)
{
    jlong tags[2048];
    jint count = 0;
    jobject *found = NULL;
    jlong *found_tags = NULL;
    jlong sum = 0;
    jint same = 0;
    jint i;
    char line[96];
    jvmtiError err;
    (void)cls;
    if (n > 2048) {
        n = 2048;
    }
    for (i = 0; i < n; i++) {
        tags[i] = base + i;
    }
    err = (*agent_env)->GetObjectsWithTags(agent_env, n, tags, &count, &found, &found_tags);
    if (err != JVMTI_ERROR_NONE) {
        snprintf(line, sizeof line, "error %d", (int)err);
        return (*env)->NewStringUTF(env, line);
    }
    for (i = 0; i < count; i++) {
        jobject expected;
        sum += found_tags[i];
        expected = (*env)->GetObjectArrayElement(env, objs, (jint)(found_tags[i] - base));
        if ((*env)->IsSameObject(env, expected, found[i])) {
            same++;
        }
        (*env)->DeleteLocalRef(env, expected);
    }
    (*agent_env)->Deallocate(agent_env, (unsigned char *)found);
    (*agent_env)->Deallocate(agent_env, (unsigned char *)found_tags);
    snprintf(line, sizeof line, "%d %lld %d", (int)count, (long long)sum, (int)same);
    return (*env)->NewStringUTF(env, line);
}

/* FollowReferences from `root`: how many tagged objects in [base, base + n)
 * are reached (each counted once, by the callback setting the tag negative). */
static jlong walk_base;
static jint walk_n;
static jint walk_hits;

static jint JNICALL on_reference(jvmtiHeapReferenceKind kind, const jvmtiHeapReferenceInfo *info,
                                 jlong class_tag, jlong referrer_class_tag, jlong size,
                                 jlong *tag_ptr, jlong *referrer_tag_ptr, jint length,
                                 void *user_data)
{
    (void)kind;
    (void)info;
    (void)class_tag;
    (void)referrer_class_tag;
    (void)size;
    (void)referrer_tag_ptr;
    (void)length;
    (void)user_data;
    if (*tag_ptr >= walk_base && *tag_ptr < walk_base + walk_n) {
        walk_hits++;
        *tag_ptr = -*tag_ptr;
    }
    return JVMTI_VISIT_OBJECTS;
}

JNIEXPORT jint JNICALL Java_L1W87TagsAcrossGc_reached(JNIEnv *env, jclass cls, jobject root,
                                                     jlong base, jint n)
{
    jvmtiHeapCallbacks callbacks;
    jvmtiError err;
    (void)env;
    (void)cls;
    memset(&callbacks, 0, sizeof callbacks);
    callbacks.heap_reference_callback = on_reference;
    walk_base = base;
    walk_n = n;
    walk_hits = 0;
    err = (*agent_env)->FollowReferences(agent_env, 0, NULL, root, &callbacks, NULL);
    if (err != JVMTI_ERROR_NONE) {
        return -(jint)err;
    }
    return walk_hits;
}

JNIEXPORT jint JNICALL Java_L1W87TagsAcrossGc_forceGc(JNIEnv *env, jclass cls)
{
    (void)env;
    (void)cls;
    return (jint)(*agent_env)->ForceGarbageCollection(agent_env);
}

JNIEXPORT jlong JNICALL Java_L1W87TagsAcrossGc_freedCount(JNIEnv *env, jclass cls)
{
    (void)env;
    (void)cls;
    return freed;
}

JNIEXPORT jlong JNICALL Java_L1W87TagsAcrossGc_freedSum(JNIEnv *env, jclass cls)
{
    (void)env;
    (void)cls;
    return freed_sum;
}
