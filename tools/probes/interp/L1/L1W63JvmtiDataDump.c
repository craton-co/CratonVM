/* Interpreter round i2 wave 63, lane L1: the shim of L1W63JvmtiDataDump.java,
 * a startup agent that counts JVMTI DataDumpRequest events (71). Linux only
 * (the event comes from SIGQUIT). */
#include <jvmti.h>
#include <string.h>

static jvmtiEnv *agent;
static volatile int dumps;

static void JNICALL on_data_dump(jvmtiEnv *jvmti) {
    (void) jvmti;
    dumps++;
}

JNIEXPORT jint JNICALL Agent_OnLoad(JavaVM *vm, char *options, void *reserved) {
    jvmtiEventCallbacks callbacks;
    (void) options;
    (void) reserved;
    if ((*vm)->GetEnv(vm, (void **) &agent, JVMTI_VERSION_1_2) != JNI_OK) {
        return 1;
    }
    memset(&callbacks, 0, sizeof callbacks);
    callbacks.DataDumpRequest = on_data_dump;
    (*agent)->SetEventCallbacks(agent, &callbacks, sizeof callbacks);
    return (*agent)->SetEventNotificationMode(agent, JVMTI_ENABLE, JVMTI_EVENT_DATA_DUMP_REQUEST, NULL);
}

JNIEXPORT jint JNICALL Java_L1W63JvmtiDataDump_dumps(JNIEnv *env, jclass cls) {
    (void) env;
    (void) cls;
    return dumps;
}
