/*
 * Native shim and agent for tools/probes/interp/L1/L1W84SetLocalObjectType.java
 * (interpreter round i4 wave 84, lane L1). Loaded twice: as an agent
 * (`-agentpath`), whose `Agent_OnLoad` adds `can_access_local_variables`
 * (an OnLoad capability), and with `System.load`, for the native `poke`.
 *
 * `poke` writes objects into its caller's locals (depth 1 of the current
 * thread) with `SetLocalObject` and answers each error code: an object whose
 * class does not fit the variable's LocalVariableTable signature is
 * JVMTI_ERROR_TYPE_MISMATCH (34), as HotSpot's check_slot_type_lvt /
 * is_assignable answer it; one that fits is JVMTI_ERROR_NONE (0).
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
    jvmtiCapabilities wanted;
    (void)options;
    (void)reserved;
    if ((*vm)->GetEnv(vm, (void **)&jvmti, JVMTI_VERSION_1_2) != JNI_OK) {
        return 1;
    }
    memset(&wanted, 0, sizeof wanted);
    wanted.can_access_local_variables = 1;
    if ((*jvmti)->AddCapabilities(jvmti, &wanted) != JVMTI_ERROR_NONE) {
        return 2;
    }
    agent_env = jvmti;
    return 0;
}

JNIEXPORT jstring JNICALL
Java_L1W84SetLocalObjectType_poke(JNIEnv *env, jclass cls, jstring text, jobject eight,
                                  jobject nine)
{
    char line[256];
    jvmtiError a, b, c, d, e, f;
    (void)cls;
    if (agent_env == NULL) {
        return (*env)->NewStringUTF(env, "no agent");
    }
    /* Slots of `target`: 0 Integer boxed, 1 Object any, 2 Runnable task,
       3 Number num, 4 int[] arr. */
    a = (*agent_env)->SetLocalObject(agent_env, NULL, 1, 0, text);
    b = (*agent_env)->SetLocalObject(agent_env, NULL, 1, 0, eight);
    c = (*agent_env)->SetLocalObject(agent_env, NULL, 1, 1, text);
    d = (*agent_env)->SetLocalObject(agent_env, NULL, 1, 2, text);
    e = (*agent_env)->SetLocalObject(agent_env, NULL, 1, 3, nine);
    f = (*agent_env)->SetLocalObject(agent_env, NULL, 1, 4, text);
    snprintf(line, sizeof line, "string->Integer=%d integer->Integer=%d string->Object=%d "
             "string->Runnable=%d integer->Number=%d string->int[]=%d",
             (int)a, (int)b, (int)c, (int)d, (int)e, (int)f);
    return (*env)->NewStringUTF(env, line);
}
