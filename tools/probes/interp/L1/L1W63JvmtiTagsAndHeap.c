/*
 * Native shim of tools/probes/interp/L1/L1W63JvmtiTagsAndHeap.java: the
 * startup agent adds `can_tag_objects`; the natives tag objects and walk the
 * heap. The Rust port (tools/probes/interp/L1/rust-ports/L1W63JvmtiTagsAndHeap)
 * makes the same calls in the same order and prints the same lines.
 */
#include <jvmti.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define NO_ENV "no JVMTI env (load the shim with -agentpath too)\n"

static jvmtiEnv *agent;

static void append(char *buf, size_t size, const char *fmt, ...) {
    va_list ap;
    size_t len = strlen(buf);
    va_start(ap, fmt);
    vsnprintf(buf + len, size - len, fmt, ap);
    va_end(ap);
}

JNIEXPORT jint JNICALL Agent_OnLoad(JavaVM *vm, char *options, void *reserved) {
    jvmtiCapabilities caps;
    if ((*vm)->GetEnv(vm, (void **)&agent, JVMTI_VERSION_1_2) != JNI_OK) return 1;
    memset(&caps, 0, sizeof caps);
    caps.can_tag_objects = 1;
    (*agent)->AddCapabilities(agent, &caps);
    return 0;
}

#define NEED_AGENT(env) if (agent == NULL) return (*env)->NewStringUTF(env, NO_ENV)

JNIEXPORT jlong JNICALL Java_L1W63JvmtiTagsAndHeap_getTag(JNIEnv *env, jclass cls, jobject o) {
    jlong tag = 0;
    jvmtiError e = (*agent)->GetTag(agent, o, &tag);
    return e != JVMTI_ERROR_NONE ? -(jlong)e : tag;
}

JNIEXPORT jint JNICALL Java_L1W63JvmtiTagsAndHeap_setTag(JNIEnv *env, jclass cls, jobject o, jlong tag) {
    return (*agent)->SetTag(agent, o, tag);
}

JNIEXPORT jstring JNICALL Java_L1W63JvmtiTagsAndHeap_tagErrors(JNIEnv *env, jclass cls, jobject any) {
    char out[2048] = "";
    jlong tag = 0;
    jint n = 0;
    jobject *objs = NULL;
    jlong one[1] = {1}, zero[1] = {0};
    NEED_AGENT(env);
    append(out, sizeof out, "GetTag null object: err=%d\n", (*agent)->GetTag(agent, NULL, &tag));
    append(out, sizeof out, "GetTag null tag_ptr: err=%d\n", (*agent)->GetTag(agent, any, NULL));
    append(out, sizeof out, "SetTag null object: err=%d\n", (*agent)->SetTag(agent, NULL, 1));
    append(out, sizeof out, "GetObjectsWithTags negative: err=%d\n", (*agent)->GetObjectsWithTags(agent, -1, one, &n, &objs, NULL));
    append(out, sizeof out, "GetObjectsWithTags null tags: err=%d\n", (*agent)->GetObjectsWithTags(agent, 1, NULL, &n, &objs, NULL));
    append(out, sizeof out, "GetObjectsWithTags tag 0: err=%d\n", (*agent)->GetObjectsWithTags(agent, 1, zero, &n, &objs, NULL));
    append(out, sizeof out, "GetObjectsWithTags null count: err=%d\n", (*agent)->GetObjectsWithTags(agent, 1, one, NULL, &objs, NULL));
    return (*env)->NewStringUTF(env, out);
}

static int cmp_long(const void *a, const void *b) {
    jlong x = *(const jlong *)a, y = *(const jlong *)b;
    return x < y ? -1 : x > y;
}

JNIEXPORT jstring JNICALL Java_L1W63JvmtiTagsAndHeap_objectsWithTags(JNIEnv *env, jclass cls, jlongArray tags) {
    char out[2048] = "";
    jint n = (*env)->GetArrayLength(env, tags), count = 0, i;
    jlong *elems = (*env)->GetLongArrayElements(env, tags, NULL);
    jobject *objs = NULL;
    jlong *found = NULL;
    int non_null = 1;
    jvmtiError e;
    NEED_AGENT(env);
    e = (*agent)->GetObjectsWithTags(agent, n, elems, &count, &objs, &found);
    (*env)->ReleaseLongArrayElements(env, tags, elems, JNI_ABORT);
    if (e != JVMTI_ERROR_NONE) {
        snprintf(out, sizeof out, "err=%d", e);
        return (*env)->NewStringUTF(env, out);
    }
    qsort(found, (size_t)count, sizeof(jlong), cmp_long);
    for (i = 0; i < count; i++) non_null &= objs[i] != NULL;
    append(out, sizeof out, "err=0 count=%d objects=%d tags=[", count, non_null);
    for (i = 0; i < count; i++) append(out, sizeof out, "%s%lld", i ? "," : "", (long long)found[i]);
    append(out, sizeof out, "]");
    (*agent)->Deallocate(agent, (unsigned char *)objs);
    (*agent)->Deallocate(agent, (unsigned char *)found);
    return (*env)->NewStringUTF(env, out);
}

JNIEXPORT jint JNICALL Java_L1W63JvmtiTagsAndHeap_forceGc(JNIEnv *env, jclass cls) {
    return (*agent)->ForceGarbageCollection(agent);
}

/* ---- IterateThroughHeap -------------------------------------------------- */

typedef struct {
    int count, tagged, abort_after;
    jlong retag, want;
    jlong sizes[64];
    int nsizes;
    jint lengths[64];
    int nlengths;
    jlong class_tags[64];
    int nclass_tags;
    char *rows[256];
    int nrows;
} Seen;

static void add_long(jlong *list, int *n, jlong v) {
    int i;
    for (i = 0; i < *n; i++) if (list[i] == v) return;
    if (*n < 64) list[(*n)++] = v;
}

static void add_int(jint *list, int *n, jint v) {
    int i;
    for (i = 0; i < *n; i++) if (list[i] == v) return;
    if (*n < 64) list[(*n)++] = v;
}

static void add_row(Seen *seen, const char *row) {
    if (seen->nrows < 256) seen->rows[seen->nrows++] = strdup(row);
}

static int cmp_int(const void *a, const void *b) {
    jint x = *(const jint *)a, y = *(const jint *)b;
    return x < y ? -1 : x > y;
}

static int cmp_str(const void *a, const void *b) { return strcmp(*(char *const *)a, *(char *const *)b); }

static void sorted_longs(jlong *list, int n, char *out, size_t size) {
    int i;
    qsort(list, (size_t)n, sizeof(jlong), cmp_long);
    out[0] = 0;
    for (i = 0; i < n; i++) append(out, size, "%s%lld", i ? "," : "", (long long)list[i]);
}

static void sorted_ints(jint *list, int n, char *out, size_t size) {
    int i;
    qsort(list, (size_t)n, sizeof(jint), cmp_int);
    out[0] = 0;
    for (i = 0; i < n; i++) append(out, size, "%s%d", i ? "," : "", list[i]);
}

static jint JNICALL iteration_cb(jlong class_tag, jlong size, jlong *tag_ptr, jint length, void *user) {
    Seen *seen = (Seen *)user;
    seen->count++;
    if (*tag_ptr != 0) seen->tagged++;
    add_long(seen->sizes, &seen->nsizes, size);
    add_int(seen->lengths, &seen->nlengths, length);
    add_long(seen->class_tags, &seen->nclass_tags, class_tag);
    if (seen->retag != 0) *tag_ptr = seen->retag;
    if (seen->abort_after != 0 && seen->count >= seen->abort_after) return JVMTI_VISIT_ABORT;
    return 0;
}

JNIEXPORT jstring JNICALL Java_L1W63JvmtiTagsAndHeap_throughHeap(JNIEnv *env, jclass cls, jint filter, jclass klass,
                                                                 jlong retag, jint abort_after) {
    jvmtiHeapCallbacks callbacks;
    Seen seen;
    char sizes[512], lengths[512], class_tags[512], out[2048];
    jvmtiError e;
    NEED_AGENT(env);
    memset(&callbacks, 0, sizeof callbacks);
    memset(&seen, 0, sizeof seen);
    callbacks.heap_iteration_callback = iteration_cb;
    seen.retag = retag;
    seen.abort_after = abort_after;
    e = (*agent)->IterateThroughHeap(agent, filter, klass, &callbacks, &seen);
    sorted_longs(seen.sizes, seen.nsizes, sizes, sizeof sizes);
    sorted_ints(seen.lengths, seen.nlengths, lengths, sizeof lengths);
    sorted_longs(seen.class_tags, seen.nclass_tags, class_tags, sizeof class_tags);
    snprintf(out, sizeof out, "err=%d count=%d tagged=%d sizes=[%s] lengths=[%s] class-tags=[%s]", e, seen.count,
             seen.tagged, sizes, lengths, class_tags);
    return (*env)->NewStringUTF(env, out);
}

static jint JNICALL array_cb(jlong class_tag, jlong size, jlong *tag_ptr, jint count, jvmtiPrimitiveType element_type,
                             const void *elements, void *user) {
    Seen *seen = (Seen *)user;
    if (*tag_ptr == seen->want) {
        char row[256];
        long long sum = 0;
        jint i;
        if (element_type == JVMTI_PRIMITIVE_TYPE_INT) {
            for (i = 0; i < count; i++) sum += ((const jint *)elements)[i];
        }
        snprintf(row, sizeof row, "array size=%lld count=%d type=%d sum=%lld", (long long)size, count, element_type, sum);
        add_row(seen, row);
    }
    return 0;
}

static jint JNICALL string_cb(jlong class_tag, jlong size, jlong *tag_ptr, const jchar *value, jint length, void *user) {
    Seen *seen = (Seen *)user;
    if (*tag_ptr == seen->want) {
        char row[256], text[128];
        jint i;
        for (i = 0; i < length && i < 127; i++) text[i] = (char)value[i];
        text[i] = 0;
        snprintf(row, sizeof row, "string size=%lld length=%d value=%s", (long long)size, length, text);
        add_row(seen, row);
    }
    return 0;
}

static jint JNICALL field_cb(jvmtiHeapReferenceKind kind, const jvmtiHeapReferenceInfo *info, jlong class_tag,
                             jlong *tag_ptr, jvalue value, jvmtiPrimitiveType value_type, void *user) {
    Seen *seen = (Seen *)user;
    if (*tag_ptr == seen->want) {
        char row[256], shown[64];
        switch (value_type) {
        case JVMTI_PRIMITIVE_TYPE_BOOLEAN: snprintf(shown, sizeof shown, "%s", value.z ? "true" : "false"); break;
        case JVMTI_PRIMITIVE_TYPE_BYTE: snprintf(shown, sizeof shown, "%d", value.b); break;
        case JVMTI_PRIMITIVE_TYPE_CHAR: snprintf(shown, sizeof shown, "%d", value.c); break;
        case JVMTI_PRIMITIVE_TYPE_SHORT: snprintf(shown, sizeof shown, "%d", value.s); break;
        case JVMTI_PRIMITIVE_TYPE_INT: snprintf(shown, sizeof shown, "%d", value.i); break;
        case JVMTI_PRIMITIVE_TYPE_LONG: snprintf(shown, sizeof shown, "%lld", (long long)value.j); break;
        case JVMTI_PRIMITIVE_TYPE_FLOAT: snprintf(shown, sizeof shown, "%g", value.f); break;
        default: snprintf(shown, sizeof shown, "%g", value.d); break;
        }
        snprintf(row, sizeof row, "field kind=%d index=%d type=%d value=%s", kind, info->field.index, value_type, shown);
        add_row(seen, row);
    }
    return 0;
}

static void joined_rows(Seen *seen, const char *sep, const char *prefix, const char *suffix, char *out, size_t size) {
    int i;
    qsort(seen->rows, (size_t)seen->nrows, sizeof(char *), cmp_str);
    for (i = 0; i < seen->nrows; i++) {
        append(out, size, "%s%s%s%s", (i && sep) ? sep : "", prefix, seen->rows[i], suffix);
        free(seen->rows[i]);
    }
}

JNIEXPORT jstring JNICALL Java_L1W63JvmtiTagsAndHeap_primitives(JNIEnv *env, jclass cls, jclass klass, jlong tag) {
    jvmtiHeapCallbacks callbacks;
    Seen seen;
    char out[4096] = "";
    jvmtiError e;
    NEED_AGENT(env);
    memset(&callbacks, 0, sizeof callbacks);
    memset(&seen, 0, sizeof seen);
    callbacks.primitive_field_callback = field_cb;
    callbacks.array_primitive_value_callback = array_cb;
    callbacks.string_primitive_value_callback = string_cb;
    seen.want = tag;
    e = (*agent)->IterateThroughHeap(agent, 0, klass, &callbacks, &seen);
    append(out, sizeof out, "err=%d [", e);
    joined_rows(&seen, "; ", "", "", out, sizeof out);
    append(out, sizeof out, "]");
    return (*env)->NewStringUTF(env, out);
}

/* ---- FollowReferences ------------------------------------------------------ */

static jint JNICALL reference_cb(jvmtiHeapReferenceKind kind, const jvmtiHeapReferenceInfo *info, jlong class_tag,
                                 jlong referrer_class_tag, jlong size, jlong *tag_ptr, jlong *referrer_tag_ptr,
                                 jint length, void *user) {
    Seen *seen = (Seen *)user;
    jlong tag = *tag_ptr;
    jlong referrer = referrer_tag_ptr == NULL ? -1 : *referrer_tag_ptr;
    seen->count++;
    if (tag != 0) {
        char row[256], index[32];
        if (kind == JVMTI_HEAP_REFERENCE_FIELD || kind == JVMTI_HEAP_REFERENCE_ARRAY_ELEMENT ||
            kind == JVMTI_HEAP_REFERENCE_STATIC_FIELD || kind == JVMTI_HEAP_REFERENCE_CONSTANT_POOL) {
            snprintf(index, sizeof index, "%d", info->field.index);
        } else {
            snprintf(index, sizeof index, "-");
        }
        snprintf(row, sizeof row, "kind=%d %lld->%lld index=%s size=%lld length=%d", kind, (long long)referrer,
                 (long long)tag, index, (long long)size, length);
        add_row(seen, row);
    }
    if (seen->abort_after != 0 && seen->count >= seen->abort_after) return JVMTI_VISIT_ABORT;
    return tag != 0 ? JVMTI_VISIT_OBJECTS : 0;
}

JNIEXPORT jstring JNICALL Java_L1W63JvmtiTagsAndHeap_follow(JNIEnv *env, jclass cls, jclass klass, jobject initial, jint abort_after) {
    jvmtiHeapCallbacks callbacks;
    Seen seen;
    char out[8192] = "";
    jvmtiError e;
    NEED_AGENT(env);
    memset(&callbacks, 0, sizeof callbacks);
    memset(&seen, 0, sizeof seen);
    callbacks.heap_reference_callback = reference_cb;
    seen.abort_after = abort_after;
    e = (*agent)->FollowReferences(agent, 0, klass, initial, &callbacks, &seen);
    if (abort_after != 0) {
        int i;
        for (i = 0; i < seen.nrows; i++) free(seen.rows[i]);
        snprintf(out, sizeof out, "follow abort=%d: err=%d calls=%d\n", abort_after, e, seen.count);
        return (*env)->NewStringUTF(env, out);
    }
    append(out, sizeof out, "follow klass=%s abort=%d: err=%d calls-to-tagged=%d\n", klass == NULL ? "null" : "set",
           abort_after, e, seen.nrows);
    joined_rows(&seen, NULL, "  ", "\n", out, sizeof out);
    return (*env)->NewStringUTF(env, out);
}

static jint JNICALL roots_cb(jvmtiHeapReferenceKind kind, const jvmtiHeapReferenceInfo *info, jlong class_tag,
                             jlong referrer_class_tag, jlong size, jlong *tag_ptr, jlong *referrer_tag_ptr,
                             jint length, void *user) {
    Seen *seen = (Seen *)user;
    jlong tag = *tag_ptr;
    if (tag >= 10 && tag <= 15) {
        add_long(seen->class_tags, &seen->nclass_tags, tag);
        if (tag == 10) add_int(seen->lengths, &seen->nlengths, kind);
    }
    return JVMTI_VISIT_OBJECTS;
}

JNIEXPORT jstring JNICALL Java_L1W63JvmtiTagsAndHeap_followRoots(JNIEnv *env, jclass cls, jclass klass) {
    jvmtiHeapCallbacks callbacks;
    Seen seen;
    char tags[512], kinds[512], out[2048];
    jvmtiError e;
    NEED_AGENT(env);
    memset(&callbacks, 0, sizeof callbacks);
    memset(&seen, 0, sizeof seen);
    callbacks.heap_reference_callback = roots_cb;
    e = (*agent)->FollowReferences(agent, 0, klass, NULL, &callbacks, &seen);
    sorted_longs(seen.class_tags, seen.nclass_tags, tags, sizeof tags);
    sorted_ints(seen.lengths, seen.nlengths, kinds, sizeof kinds);
    snprintf(out, sizeof out, "follow roots: err=%d tags-reached=[%s] kinds-to-root-node=[%s]\n", e, tags, kinds);
    return (*env)->NewStringUTF(env, out);
}

/* ---- JVMTI 1.0 walks ---------------------------------------------------------- */

static jvmtiIterationControl JNICALL old_object_cb(jlong class_tag, jlong size, jlong *tag_ptr, void *user) {
    Seen *seen = (Seen *)user;
    seen->count++;
    if (*tag_ptr != 0) seen->tagged++;
    add_long(seen->sizes, &seen->nsizes, size);
    return JVMTI_ITERATION_CONTINUE;
}

static jvmtiIterationControl JNICALL old_reference_cb(jvmtiObjectReferenceKind kind, jlong class_tag, jlong size,
                                                      jlong *tag_ptr, jlong referrer_tag, jint referrer_index, void *user) {
    Seen *seen = (Seen *)user;
    jlong tag = *tag_ptr;
    if (tag != 0) {
        char row[256];
        snprintf(row, sizeof row, "kind=%d %lld->%lld index=%d", kind, (long long)referrer_tag, (long long)tag, referrer_index);
        add_row(seen, row);
        return JVMTI_ITERATION_CONTINUE;
    }
    return JVMTI_ITERATION_IGNORE;
}

static jvmtiIterationControl JNICALL old_root_cb(jvmtiHeapRootKind kind, jlong class_tag, jlong size, jlong *tag_ptr, void *user) {
    Seen *seen = (Seen *)user;
    if (*tag_ptr != 0) add_long(seen->class_tags, &seen->nclass_tags, *tag_ptr);
    return JVMTI_ITERATION_CONTINUE;
}

static jvmtiIterationControl JNICALL old_stack_cb(jvmtiHeapRootKind kind, jlong class_tag, jlong size, jlong *tag_ptr,
                                                  jlong thread_tag, jint depth, jmethodID method, jint slot, void *user) {
    Seen *seen = (Seen *)user;
    if (*tag_ptr != 0) add_long(seen->class_tags, &seen->nclass_tags, *tag_ptr);
    return JVMTI_ITERATION_CONTINUE;
}

static jvmtiIterationControl JNICALL old_reach_cb(jvmtiObjectReferenceKind kind, jlong class_tag, jlong size,
                                                  jlong *tag_ptr, jlong referrer_tag, jint referrer_index, void *user) {
    Seen *seen = (Seen *)user;
    jlong tag = *tag_ptr;
    if (tag >= 10 && tag <= 15) add_long(seen->class_tags, &seen->nclass_tags, tag);
    return JVMTI_ITERATION_CONTINUE;
}

JNIEXPORT jstring JNICALL Java_L1W63JvmtiTagsAndHeap_oldWalks(JNIEnv *env, jclass cls, jclass klass, jobject initial) {
    char out[8192] = "", list[512];
    Seen seen;
    jvmtiError e;
    int i, kept = 0;
    NEED_AGENT(env);
    memset(&seen, 0, sizeof seen);
    e = (*agent)->IterateOverHeap(agent, JVMTI_HEAP_OBJECT_TAGGED, old_object_cb, &seen);
    append(out, sizeof out, "IterateOverHeap tagged: err=%d count-at-least-7=%d\n", e, seen.count >= 7);
    memset(&seen, 0, sizeof seen);
    e = (*agent)->IterateOverInstancesOfClass(agent, klass, JVMTI_HEAP_OBJECT_EITHER, old_object_cb, &seen);
    sorted_longs(seen.sizes, seen.nsizes, list, sizeof list);
    append(out, sizeof out, "IterateOverInstancesOfClass Node: err=%d count=%d tagged=%d sizes=[%s]\n", e, seen.count,
           seen.tagged, list);
    memset(&seen, 0, sizeof seen);
    e = (*agent)->IterateOverObjectsReachableFromObject(agent, initial, old_reference_cb, &seen);
    append(out, sizeof out, "IterateOverObjectsReachableFromObject: err=%d\n", e);
    joined_rows(&seen, NULL, "  ", "\n", out, sizeof out);
    memset(&seen, 0, sizeof seen);
    e = (*agent)->IterateOverReachableObjects(agent, old_root_cb, old_stack_cb, old_reach_cb, &seen);
    for (i = 0; i < seen.nclass_tags; i++) {
        if (seen.class_tags[i] >= 10 && seen.class_tags[i] <= 15) seen.class_tags[kept++] = seen.class_tags[i];
    }
    sorted_longs(seen.class_tags, kept, list, sizeof list);
    append(out, sizeof out, "IterateOverReachableObjects: err=%d tags-reached=[%s]\n", e, list);
    return (*env)->NewStringUTF(env, out);
}

JNIEXPORT jstring JNICALL Java_L1W63JvmtiTagsAndHeap_walkErrors(JNIEnv *env, jclass cls, jclass klass, jobject any) {
    char out[2048] = "";
    jvmtiHeapCallbacks callbacks;
    Seen seen;
    NEED_AGENT(env);
    memset(&callbacks, 0, sizeof callbacks);
    memset(&seen, 0, sizeof seen);
    append(out, sizeof out, "IterateThroughHeap null callbacks: err=%d\n", (*agent)->IterateThroughHeap(agent, 0, klass, NULL, NULL));
    append(out, sizeof out, "IterateThroughHeap object as klass: err=%d\n",
           (*agent)->IterateThroughHeap(agent, 0, (jclass)any, &callbacks, NULL));
    append(out, sizeof out, "FollowReferences null callbacks: err=%d\n", (*agent)->FollowReferences(agent, 0, klass, NULL, NULL, NULL));
    append(out, sizeof out, "FollowReferences object as klass: err=%d\n",
           (*agent)->FollowReferences(agent, 0, (jclass)any, NULL, &callbacks, NULL));
    append(out, sizeof out, "IterateOverHeap null callback: err=%d\n", (*agent)->IterateOverHeap(agent, JVMTI_HEAP_OBJECT_EITHER, NULL, NULL));
    append(out, sizeof out, "IterateOverHeap filter 9: err=%d\n",
           (*agent)->IterateOverHeap(agent, (jvmtiHeapObjectFilter)9, old_object_cb, &seen));
    append(out, sizeof out, "IterateOverInstancesOfClass null klass: err=%d\n",
           (*agent)->IterateOverInstancesOfClass(agent, NULL, JVMTI_HEAP_OBJECT_EITHER, old_object_cb, &seen));
    append(out, sizeof out, "IterateOverObjectsReachableFromObject null object: err=%d\n",
           (*agent)->IterateOverObjectsReachableFromObject(agent, NULL, old_reference_cb, &seen));
    return (*env)->NewStringUTF(env, out);
}
