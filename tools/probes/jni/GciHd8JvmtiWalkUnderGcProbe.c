/*
 * gci wave i8, lane hd: the agent and natives of
 * tools/probes/GciHd8JvmtiWalkUnderGcProbe.java (JVMTI heap walks while
 * collections are requested back to back). Loaded twice: as
 * -agentpath:<lib> (Agent_OnLoad takes the jvmtiEnv with can_tag_objects)
 * and with System.load(<lib>) for the natives.
 *
 *   Windows: cl /LD /O1 /I"%JDK%\include" /I"%JDK%\include\win32" GciHd8JvmtiWalkUnderGcProbe.c /Fegcihd8walk.dll
 *   Linux:   gcc -shared -fPIC -O1 -I$JDK/include -I$JDK/include/linux -o libgcihd8walk.so GciHd8JvmtiWalkUnderGcProbe.c
 */
#include <jni.h>
#include <jvmti.h>
#include <string.h>

#define NODE_CLASS_TAG 7

static jvmtiEnv *jvmti;

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

JNIEXPORT jint JNICALL Java_GciHd8JvmtiWalkUnderGcProbe_init(JNIEnv *env, jclass cls, jclass node) {
    (void)env;
    (void)cls;
    if (jvmti == NULL) {
        return -1;
    }
    return (jint)(*jvmti)->SetTag(jvmti, node, NODE_CLASS_TAG);
}

static jint JNICALL count_instance(jlong class_tag, jlong size, jlong *tag_ptr, jint length, void *user_data) {
    (void)class_tag;
    (void)size;
    (void)tag_ptr;
    (void)length;
    (*(jlong *)user_data)++;
    return 0;
}

JNIEXPORT jlong JNICALL Java_GciHd8JvmtiWalkUnderGcProbe_through(JNIEnv *env, jclass cls, jclass node) {
    jvmtiHeapCallbacks callbacks;
    jlong count = 0;
    jvmtiError err;
    (void)env;
    (void)cls;
    memset(&callbacks, 0, sizeof(callbacks));
    callbacks.heap_iteration_callback = count_instance;
    err = (*jvmti)->IterateThroughHeap(jvmti, 0, node, &callbacks, &count);
    return err == JVMTI_ERROR_NONE ? count : -(jlong)err;
}

struct follow_state {
    jlong mark;
    jlong count;
};

static jint JNICALL count_node(jvmtiHeapReferenceKind kind, const jvmtiHeapReferenceInfo *info, jlong class_tag,
                               jlong referrer_class_tag, jlong size, jlong *tag_ptr, jlong *referrer_tag_ptr,
                               jint length, void *user_data) {
    struct follow_state *st = (struct follow_state *)user_data;
    (void)kind;
    (void)info;
    (void)referrer_class_tag;
    (void)size;
    (void)referrer_tag_ptr;
    (void)length;
    if (class_tag != NODE_CLASS_TAG) {
        return 0;
    }
    if (*tag_ptr != st->mark) {
        *tag_ptr = st->mark;
        st->count++;
    }
    return JVMTI_VISIT_OBJECTS;
}

JNIEXPORT jlong JNICALL Java_GciHd8JvmtiWalkUnderGcProbe_follow(JNIEnv *env, jclass cls, jobject initial, jlong mark) {
    jvmtiHeapCallbacks callbacks;
    struct follow_state st;
    jvmtiError err;
    (void)env;
    (void)cls;
    memset(&callbacks, 0, sizeof(callbacks));
    callbacks.heap_reference_callback = count_node;
    st.mark = mark;
    st.count = 0;
    err = (*jvmti)->FollowReferences(jvmti, 0, NULL, initial, &callbacks, &st);
    return err == JVMTI_ERROR_NONE ? st.count : -(jlong)err;
}
