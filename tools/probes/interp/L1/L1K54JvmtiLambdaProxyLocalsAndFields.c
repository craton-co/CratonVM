/*
 * Native shim of tools/probes/interp/L1/L1K54JvmtiLambdaProxyLocalsAndFields.java:
 * a lambda class's fields (JVMTI facts, values through JNI with the JVMTI
 * field id), the locals of its frame, and the frames from a hot body.
 */
#include <jni.h>
#include <jvmti.h>
#include <stdio.h>
#include <string.h>

static jvmtiEnv *agent;

JNIEXPORT jint JNICALL Agent_OnLoad(JavaVM *vm, char *options, void *reserved) {
    jvmtiCapabilities caps;
    if ((*vm)->GetEnv(vm, (void **)&agent, JVMTI_VERSION_1_2) != JNI_OK) {
        return 1;
    }
    memset(&caps, 0, sizeof caps);
    caps.can_get_synthetic_attribute = 1;
    caps.can_access_local_variables = 1;
    (*agent)->AddCapabilities(agent, &caps);
    return 0;
}

static size_t put(char *out, size_t at, size_t cap, const char *text) {
    size_t n = strlen(text);
    if (at + n + 1 > cap) {
        return at;
    }
    memcpy(out + at, text, n + 1);
    return at + n;
}

static void name_of(jmethodID method, char *buf, size_t cap) {
    char *name = NULL;
    char *sig = NULL;
    if ((*agent)->GetMethodName(agent, method, &name, &sig, NULL) != JVMTI_ERROR_NONE) {
        snprintf(buf, cap, "?");
        return;
    }
    snprintf(buf, cap, "%s%s", name, sig);
    (*agent)->Deallocate(agent, (unsigned char *)name);
    (*agent)->Deallocate(agent, (unsigned char *)sig);
}

JNIEXPORT jstring JNICALL Java_L1K54JvmtiLambdaProxyLocalsAndFields_fields(JNIEnv *env, jclass cls, jobject lambda) {
    char out[2048];
    char line[512];
    size_t at = 0;
    jclass klass;
    jint count = 0;
    jfieldID *fields = NULL;
    jvmtiError err;
    int i;
    if (agent == NULL) {
        return (*env)->NewStringUTF(env, "no JVMTI env");
    }
    klass = (*env)->GetObjectClass(env, lambda);
    err = (*agent)->GetClassFields(agent, klass, &count, &fields);
    snprintf(line, sizeof line, "err=%d count=%d", (int)err, (int)(err == JVMTI_ERROR_NONE ? count : 0));
    at = put(out, at, sizeof out, line);
    for (i = 0; err == JVMTI_ERROR_NONE && i < count; i++) {
        char *name = NULL;
        char *sig = NULL;
        char *generic = NULL;
        jint mods = 0;
        jclass declaring = NULL;
        jboolean synthetic = 0;
        jvmtiError e_name = (*agent)->GetFieldName(agent, klass, fields[i], &name, &sig, &generic);
        jvmtiError e_mods = (*agent)->GetFieldModifiers(agent, klass, fields[i], &mods);
        jvmtiError e_decl = (*agent)->GetFieldDeclaringClass(agent, klass, fields[i], &declaring);
        jvmtiError e_syn = (*agent)->IsFieldSynthetic(agent, klass, fields[i], &synthetic);
        char value[256];
        if (e_name != JVMTI_ERROR_NONE || sig == NULL) {
            snprintf(value, sizeof value, "?");
        } else if (sig[0] == 'J') {
            snprintf(value, sizeof value, "%lld", (long long)(*env)->GetLongField(env, lambda, fields[i]));
        } else if (sig[0] == 'I') {
            snprintf(value, sizeof value, "%d", (int)(*env)->GetIntField(env, lambda, fields[i]));
        } else if (strcmp(sig, "Ljava/lang/String;") == 0) {
            jstring s = (jstring)(*env)->GetObjectField(env, lambda, fields[i]);
            const char *chars = s == NULL ? NULL : (*env)->GetStringUTFChars(env, s, NULL);
            snprintf(value, sizeof value, "\"%s\"", chars == NULL ? "null" : chars);
            if (chars != NULL) {
                (*env)->ReleaseStringUTFChars(env, s, chars);
            }
        } else {
            jobject o = (*env)->GetObjectField(env, lambda, fields[i]);
            char *osig = NULL;
            if (o == NULL) {
                snprintf(value, sizeof value, "null");
            } else if ((*agent)->GetClassSignature(agent, (*env)->GetObjectClass(env, o), &osig, NULL) == JVMTI_ERROR_NONE) {
                snprintf(value, sizeof value, "a %s", osig);
                (*agent)->Deallocate(agent, (unsigned char *)osig);
            } else {
                snprintf(value, sizeof value, "?");
            }
        }
        snprintf(line, sizeof line,
                 "\n[%d] name err=%d %s %s generic=%s modifiers err=%d 0x%x declaring err=%d own=%d synthetic err=%d %d value=%s",
                 i, (int)e_name, name ? name : "?", sig ? sig : "?", generic ? generic : "null", (int)e_mods, (int)mods,
                 (int)e_decl, declaring != NULL && (*env)->IsSameObject(env, declaring, klass) ? 1 : 0, (int)e_syn,
                 (int)synthetic, value);
        at = put(out, at, sizeof out, line);
        if (name != NULL) {
            (*agent)->Deallocate(agent, (unsigned char *)name);
        }
        if (sig != NULL) {
            (*agent)->Deallocate(agent, (unsigned char *)sig);
        }
        if (generic != NULL) {
            (*agent)->Deallocate(agent, (unsigned char *)generic);
        }
    }
    if (fields != NULL) {
        (*agent)->Deallocate(agent, (unsigned char *)fields);
    }
    return (*env)->NewStringUTF(env, out);
}

JNIEXPORT jstring JNICALL Java_L1K54JvmtiLambdaProxyLocalsAndFields_locals(JNIEnv *env, jclass cls, jobject lambda,
                                                                         jobject arg, jint depth) {
    char out[2048];
    char line[512];
    char name[256];
    size_t at = 0;
    jthread thread = NULL;
    jmethodID method = NULL;
    jlocation location = -1;
    jobject obj = NULL;
    jint value = 0;
    jint entries = 0;
    jvmtiLocalVariableEntry *table = NULL;
    jvmtiError err;
    if (agent == NULL) {
        return (*env)->NewStringUTF(env, "no JVMTI env");
    }
    (*agent)->GetCurrentThread(agent, &thread);
    err = (*agent)->GetFrameLocation(agent, thread, depth, &method, &location);
    name_of(method, name, sizeof name);
    snprintf(line, sizeof line, "depth %d: err=%d %s at %lld", (int)depth, (int)err, err == JVMTI_ERROR_NONE ? name : "-",
             (long long)location);
    at = put(out, at, sizeof out, line);
    obj = NULL;
    err = (*agent)->GetLocalInstance(agent, thread, depth, &obj);
    snprintf(line, sizeof line, "\ninstance: err=%d same=%d", (int)err,
             obj != NULL && (*env)->IsSameObject(env, obj, lambda) ? 1 : 0);
    at = put(out, at, sizeof out, line);
    obj = NULL;
    err = (*agent)->GetLocalObject(agent, thread, depth, 0, &obj);
    snprintf(line, sizeof line, "\nslot 0 object: err=%d same=%d", (int)err,
             obj != NULL && (*env)->IsSameObject(env, obj, lambda) ? 1 : 0);
    at = put(out, at, sizeof out, line);
    obj = NULL;
    err = (*agent)->GetLocalObject(agent, thread, depth, 1, &obj);
    snprintf(line, sizeof line, "\nslot 1 object: err=%d arg=%d", (int)err,
             obj != NULL && arg != NULL && (*env)->IsSameObject(env, obj, arg) ? 1 : 0);
    at = put(out, at, sizeof out, line);
    value = 0;
    err = (*agent)->GetLocalInt(agent, thread, depth, 1, &value);
    snprintf(line, sizeof line, "\nslot 1 int: err=%d value=%d", (int)err, (int)value);
    at = put(out, at, sizeof out, line);
    value = 0;
    err = (*agent)->GetLocalInt(agent, thread, depth, 2, &value);
    snprintf(line, sizeof line, "\nslot 2 int: err=%d value=%d", (int)err, (int)value);
    at = put(out, at, sizeof out, line);
    obj = NULL;
    err = (*agent)->GetLocalObject(agent, thread, depth, 9, &obj);
    snprintf(line, sizeof line, "\nslot 9 object: err=%d", (int)err);
    at = put(out, at, sizeof out, line);
    err = (*agent)->GetLocalVariableTable(agent, method, &entries, &table);
    snprintf(line, sizeof line, "\nvariable table: err=%d count=%d", (int)err,
             (int)(err == JVMTI_ERROR_NONE ? entries : 0));
    at = put(out, at, sizeof out, line);
    if (table != NULL) {
        (*agent)->Deallocate(agent, (unsigned char *)table);
    }
    return (*env)->NewStringUTF(env, out);
}

JNIEXPORT jstring JNICALL Java_L1K54JvmtiLambdaProxyLocalsAndFields_frames(JNIEnv *env, jclass cls) {
    char out[512];
    char two[256];
    char three[256];
    jthread thread = NULL;
    jint count = 0;
    jmethodID method = NULL;
    jlocation location = -1;
    jvmtiError e_count;
    jvmtiError e_two;
    jvmtiError e_three;
    jlocation at3 = -1;
    if (agent == NULL) {
        return (*env)->NewStringUTF(env, "no JVMTI env");
    }
    (*agent)->GetCurrentThread(agent, &thread);
    e_count = (*agent)->GetFrameCount(agent, thread, &count);
    e_two = (*agent)->GetFrameLocation(agent, thread, 2, &method, &location);
    name_of(method, two, sizeof two);
    e_three = (*agent)->GetFrameLocation(agent, thread, 3, &method, &at3);
    name_of(method, three, sizeof three);
    snprintf(out, sizeof out, "count err=%d %d; depth 2 err=%d %s at %lld; depth 3 err=%d %s", (int)e_count,
             (int)count, (int)e_two, two, (long long)location, (int)e_three, three);
    return (*env)->NewStringUTF(env, out);
}
