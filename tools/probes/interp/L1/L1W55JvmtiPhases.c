/*
 * Native shim and agent for tools/probes/interp/L1/L1W55JvmtiPhases.java
 * (interpreter round i2 wave 55, lane L1). Loaded twice: as an agent
 * (`-agentpath:...`), whose `Agent_OnLoad` enables `VMStart`, `VMInit`,
 * `VMDeath`, `ClassLoad`, `ClassPrepare` and `ThreadStart`, and with
 * `System.load`, for the natives `rows` and `phase`. Build instructions are
 * in the probe's header; the Rust port
 * tools/probes/interp/L1/rust-ports/L1W55JvmtiPhases prints the same lines.
 */
#include <jni.h>
#include <jvmti.h>
#include <stdarg.h>
#include <stdio.h>
#include <string.h>

static char out[8192];
static jvmtiEnv *agent_env = NULL;
static int vmstart_seen = 0;
static int vmstart_phase = 0;
static int vminit_seen = 0;
static int vminit_phase = 0;
static int vminit_after_start = 0;
static int early_load = 0;
static int early_prepare = 0;
static int early_thread = 0;
static int start_calls[5] = { -1, -1, -1, -1, -1 };

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

static int phase_of(jvmtiEnv *jvmti)
{
    jvmtiPhase phase = (jvmtiPhase)0;
    (*jvmti)->GetPhase(jvmti, &phase);
    return (int)phase;
}

static void JNICALL
on_vm_start(jvmtiEnv *jvmti, JNIEnv *env)
{
    jthread current = NULL;
    jint count = 0;
    jthread *threads = NULL;
    jclass *classes = NULL;
    jclass object;
    char *sig = NULL;
    vmstart_seen++;
    vmstart_phase = phase_of(jvmti);
    start_calls[0] = (int)(*jvmti)->GetCurrentThread(jvmti, &current);
    start_calls[1] = (int)(*jvmti)->GetAllThreads(jvmti, &count, &threads);
    if (start_calls[1] == JVMTI_ERROR_NONE) {
        (*jvmti)->Deallocate(jvmti, (unsigned char *)threads);
    }
    start_calls[2] = (int)(*jvmti)->GetLoadedClasses(jvmti, &count, &classes);
    if (start_calls[2] == JVMTI_ERROR_NONE) {
        (*jvmti)->Deallocate(jvmti, (unsigned char *)classes);
    }
    object = (*env)->FindClass(env, "java/lang/Object");
    if (object == NULL) {
        (*env)->ExceptionClear(env);
    } else {
        start_calls[3] = (int)(*jvmti)->GetClassSignature(jvmti, object, &sig, NULL);
        if (start_calls[3] == JVMTI_ERROR_NONE) {
            (*jvmti)->Deallocate(jvmti, (unsigned char *)sig);
        }
    }
    start_calls[4] = (int)(*jvmti)->SetEventNotificationMode(jvmti, JVMTI_ENABLE,
                                                             JVMTI_EVENT_VM_START, NULL);
}

static void JNICALL
on_vm_init(jvmtiEnv *jvmti, JNIEnv *env, jthread thread)
{
    (void)env;
    (void)thread;
    vminit_seen++;
    vminit_phase = phase_of(jvmti);
    vminit_after_start = vmstart_seen > 0;
}

static void JNICALL
on_vm_death(jvmtiEnv *jvmti, JNIEnv *env)
{
    (void)env;
    printf("vmdeath: phase=%d\n", phase_of(jvmti));
    fflush(stdout);
}

static void JNICALL
on_class_load(jvmtiEnv *jvmti, JNIEnv *env, jthread thread, jclass klass)
{
    (void)jvmti;
    (void)env;
    (void)thread;
    (void)klass;
    if (vmstart_seen == 0) {
        early_load++;
    }
}

static void JNICALL
on_class_prepare(jvmtiEnv *jvmti, JNIEnv *env, jthread thread, jclass klass)
{
    (void)jvmti;
    (void)env;
    (void)thread;
    (void)klass;
    if (vmstart_seen == 0) {
        early_prepare++;
    }
}

static void JNICALL
on_thread_start(jvmtiEnv *jvmti, JNIEnv *env, jthread thread)
{
    (void)jvmti;
    (void)env;
    (void)thread;
    if (vmstart_seen == 0) {
        early_thread++;
    }
}

JNIEXPORT jint JNICALL
Agent_OnLoad(JavaVM *vm, char *options, void *reserved)
{
    jvmtiEnv *jvmti = NULL;
    jvmtiEventCallbacks callbacks;
    int phase;
    int set;
    int codes[6];
    static const jvmtiEvent events[6] = {
        JVMTI_EVENT_VM_START, JVMTI_EVENT_VM_INIT, JVMTI_EVENT_VM_DEATH,
        JVMTI_EVENT_CLASS_LOAD, JVMTI_EVENT_CLASS_PREPARE, JVMTI_EVENT_THREAD_START
    };
    int i;
    (void)options;
    (void)reserved;
    if ((*vm)->GetEnv(vm, (void **)&jvmti, JVMTI_VERSION_1_2) != JNI_OK) {
        return 1;
    }
    out[0] = '\0';
    phase = phase_of(jvmti);
    memset(&callbacks, 0, sizeof callbacks);
    callbacks.VMStart = &on_vm_start;
    callbacks.VMInit = &on_vm_init;
    callbacks.VMDeath = &on_vm_death;
    callbacks.ClassLoad = &on_class_load;
    callbacks.ClassPrepare = &on_class_prepare;
    callbacks.ThreadStart = &on_thread_start;
    set = (int)(*jvmti)->SetEventCallbacks(jvmti, &callbacks, (jint)sizeof callbacks);
    for (i = 0; i < 6; i++) {
        codes[i] = (int)(*jvmti)->SetEventNotificationMode(jvmti, JVMTI_ENABLE, events[i], NULL);
    }
    line("onload: phase=%d callbacks=%d start=%d init=%d death=%d load=%d prepare=%d thread=%d",
         phase, set, codes[0], codes[1], codes[2], codes[3], codes[4], codes[5]);
    {
        jvmtiCapabilities potential;
        jvmtiCapabilities wanted;
        int err;
        memset(&potential, 0, sizeof potential);
        err = (int)(*jvmti)->GetPotentialCapabilities(jvmti, &potential);
        memset(&wanted, 0, sizeof wanted);
        wanted.can_generate_all_class_hook_events = 1;
        line("onload all class hook events: potential err=%d bit=%d add=%d", err,
             (int)potential.can_generate_all_class_hook_events,
             (int)(*jvmti)->AddCapabilities(jvmti, &wanted));
    }
    agent_env = jvmti;
    return 0;
}

JNIEXPORT void JNICALL
Agent_OnUnload(JavaVM *vm)
{
    (void)vm;
    if (agent_env != NULL) {
        printf("unload: phase=%d\n", phase_of(agent_env));
        fflush(stdout);
    }
}

JNIEXPORT jstring JNICALL
Java_L1W55JvmtiPhases_rows(JNIEnv *env, jclass self)
{
    jstring answer;
    (void)self;
    if (agent_env == NULL) {
        return (*env)->NewStringUTF(env, "no JVMTI env (load the shim with -agentpath too)\n");
    }
    line("vmstart: seen=%d phase=%d", vmstart_seen, vmstart_phase);
    line("before vmstart: load=%s prepare=%s thread=%s", early_load ? "yes" : "no",
         early_prepare ? "yes" : "no", early_thread ? "yes" : "no");
    line("start phase calls: current=%d threads=%d loaded=%d signature=%d mode=%d",
         start_calls[0], start_calls[1], start_calls[2], start_calls[3], start_calls[4]);
    line("vminit: seen=%d phase=%d after vmstart=%s", vminit_seen, vminit_phase,
         vminit_after_start ? "yes" : "no");
    answer = (*env)->NewStringUTF(env, out);
    out[0] = '\0';
    return answer;
}

/* A second env, obtained in the live phase: whether
 * can_generate_all_class_hook_events is potential there and can be added. */
JNIEXPORT jstring JNICALL
Java_L1W55JvmtiPhases_livePotential(JNIEnv *env, jclass self)
{
    JavaVM *vm = NULL;
    jvmtiEnv *other = NULL;
    jvmtiCapabilities potential;
    jvmtiCapabilities wanted;
    char text[96];
    int err;
    (void)self;
    if ((*env)->GetJavaVM(env, &vm) != JNI_OK
        || (*vm)->GetEnv(vm, (void **)&other, JVMTI_VERSION_1_2) != JNI_OK) {
        return (*env)->NewStringUTF(env, "no second env");
    }
    memset(&potential, 0, sizeof potential);
    err = (int)(*other)->GetPotentialCapabilities(other, &potential);
    memset(&wanted, 0, sizeof wanted);
    wanted.can_generate_all_class_hook_events = 1;
    snprintf(text, sizeof text, "potential err=%d bit=%d add=%d", err,
             (int)potential.can_generate_all_class_hook_events,
             (int)(*other)->AddCapabilities(other, &wanted));
    (*other)->DisposeEnvironment(other);
    return (*env)->NewStringUTF(env, text);
}

JNIEXPORT jint JNICALL
Java_L1W55JvmtiPhases_phase(JNIEnv *env, jclass self)
{
    (void)env;
    (void)self;
    return agent_env == NULL ? -1 : phase_of(agent_env);
}
