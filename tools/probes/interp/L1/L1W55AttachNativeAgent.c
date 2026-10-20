/*
 * Native shim and agent for tools/probes/interp/L1/L1W55AttachNativeAgent.java
 * (interpreter round i2 wave 55, lane L1). Loaded with `System.load`, for the
 * native `rows`, and then by the probe's own self-attach
 * (`VirtualMachine.loadAgentPath`), whose `Agent_OnAttach` records its
 * options, the phase of an env it obtains, and answers 3 for the options
 * "fail". Build instructions are in the probe's header; the Rust port
 * tools/probes/interp/L1/rust-ports/L1W55AttachNativeAgent prints the same lines.
 */
#include <jni.h>
#include <jvmti.h>
#include <stdarg.h>
#include <stdio.h>
#include <string.h>

static char out[4096];
static int attaches = 0;

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
Agent_OnAttach(JavaVM *vm, char *options, void *reserved)
{
    jvmtiEnv *jvmti = NULL;
    jvmtiPhase phase = (jvmtiPhase)0;
    int got;
    (void)reserved;
    attaches++;
    got = (int)(*vm)->GetEnv(vm, (void **)&jvmti, JVMTI_VERSION_1_2);
    if (got == JNI_OK) {
        (*jvmti)->GetPhase(jvmti, &phase);
        (*jvmti)->DisposeEnvironment(jvmti);
    }
    line("onattach %d: options=%s getenv=%d phase=%d", attaches,
         options == NULL ? "NULL" : options, got, (int)phase);
    return options != NULL && strcmp(options, "fail") == 0 ? 3 : 0;
}

JNIEXPORT jstring JNICALL
Java_L1W55AttachNativeAgent_rows(JNIEnv *env, jclass self)
{
    jstring answer;
    (void)self;
    answer = (*env)->NewStringUTF(env, out);
    out[0] = '\0';
    return answer;
}
