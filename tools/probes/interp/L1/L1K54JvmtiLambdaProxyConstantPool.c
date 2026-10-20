/*
 * Native shim of tools/probes/interp/L1/L1K54JvmtiLambdaProxyConstantPool.java:
 * GetConstantPool of a lambda's class, decoded entry by entry (the hidden
 * class's suffix masked), and GetBytecodes of each of its methods in hex.
 */
#include <jni.h>
#include <jvmti.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static jvmtiEnv *agent;

JNIEXPORT jint JNICALL Agent_OnLoad(JavaVM *vm, char *options, void *reserved) {
    jvmtiCapabilities caps;
    if ((*vm)->GetEnv(vm, (void **)&agent, JVMTI_VERSION_1_2) != JNI_OK) {
        return 1;
    }
    memset(&caps, 0, sizeof caps);
    caps.can_get_bytecodes = 1;
    caps.can_get_constant_pool = 1;
    (*agent)->AddCapabilities(agent, &caps);
    return 0;
}

/* `text` with `0x` and the hex digits after it as `0x?`. */
static void mask(const char *in, size_t len, char *out, size_t cap) {
    size_t at = 0;
    size_t i = 0;
    while (i < len && at + 4 < cap) {
        if (i + 1 < len && in[i] == '0' && in[i + 1] == 'x') {
            out[at++] = '0';
            out[at++] = 'x';
            out[at++] = '?';
            i += 2;
            while (i < len && ((in[i] >= '0' && in[i] <= '9') || (in[i] >= 'a' && in[i] <= 'f') ||
                               (in[i] >= 'A' && in[i] <= 'F'))) {
                i++;
            }
        } else {
            out[at++] = in[i++];
        }
    }
    out[at] = '\0';
}

static unsigned u2(const unsigned char *p) {
    return ((unsigned)p[0] << 8) | p[1];
}

static size_t put(char *out, size_t at, size_t cap, const char *text) {
    size_t n = strlen(text);
    if (at + n + 1 > cap) {
        return at;
    }
    memcpy(out + at, text, n + 1);
    return at + n;
}

JNIEXPORT jstring JNICALL Java_L1K54JvmtiLambdaProxyConstantPool_dump(JNIEnv *env, jclass cls, jobject lambda) {
    static char out[65536];
    char line[1024];
    size_t at = 0;
    jclass klass = (*env)->GetObjectClass(env, lambda);
    jint count = 0;
    jint nbytes = 0;
    unsigned char *bytes = NULL;
    jvmtiError err = (*agent)->GetConstantPool(agent, klass, &count, &nbytes, &bytes);
    jint mcount = 0;
    jmethodID *methods = NULL;
    int k;
    out[0] = '\0';
    snprintf(line, sizeof line, "cp err=%d count=%d bytes=%d", (int)err, (int)count, (int)nbytes);
    at = put(out, at, sizeof out, line);
    if (err == JVMTI_ERROR_NONE) {
        const unsigned char *p = bytes;
        int i = 1;
        while (i < count && p < bytes + nbytes) {
            int wide = 0;
            switch (p[0]) {
            case 1: {
                char masked[512];
                unsigned n = u2(p + 1);
                mask((const char *)p + 3, n, masked, sizeof masked);
                snprintf(line, sizeof line, "\n#%d Utf8 %s", i, masked);
                p += 3 + n;
                break;
            }
            case 3:
                snprintf(line, sizeof line, "\n#%d Integer %d", i,
                         (int)(((unsigned)p[1] << 24) | ((unsigned)p[2] << 16) | ((unsigned)p[3] << 8) | p[4]));
                p += 5;
                break;
            case 4:
                snprintf(line, sizeof line, "\n#%d Float", i);
                p += 5;
                break;
            case 5:
                snprintf(line, sizeof line, "\n#%d Long", i);
                p += 9;
                wide = 1;
                break;
            case 6:
                snprintf(line, sizeof line, "\n#%d Double", i);
                p += 9;
                wide = 1;
                break;
            case 7:
                snprintf(line, sizeof line, "\n#%d Class #%u", i, u2(p + 1));
                p += 3;
                break;
            case 8:
                snprintf(line, sizeof line, "\n#%d String #%u", i, u2(p + 1));
                p += 3;
                break;
            case 9:
                snprintf(line, sizeof line, "\n#%d Fieldref #%u #%u", i, u2(p + 1), u2(p + 3));
                p += 5;
                break;
            case 10:
                snprintf(line, sizeof line, "\n#%d Methodref #%u #%u", i, u2(p + 1), u2(p + 3));
                p += 5;
                break;
            case 11:
                snprintf(line, sizeof line, "\n#%d InterfaceMethodref #%u #%u", i, u2(p + 1), u2(p + 3));
                p += 5;
                break;
            case 12:
                snprintf(line, sizeof line, "\n#%d NameAndType #%u #%u", i, u2(p + 1), u2(p + 3));
                p += 5;
                break;
            case 15:
                snprintf(line, sizeof line, "\n#%d MethodHandle %u #%u", i, (unsigned)p[1], u2(p + 2));
                p += 4;
                break;
            case 16:
                snprintf(line, sizeof line, "\n#%d MethodType #%u", i, u2(p + 1));
                p += 3;
                break;
            case 17:
                snprintf(line, sizeof line, "\n#%d Dynamic %u #%u", i, u2(p + 1), u2(p + 3));
                p += 5;
                break;
            case 18:
                snprintf(line, sizeof line, "\n#%d InvokeDynamic %u #%u", i, u2(p + 1), u2(p + 3));
                p += 5;
                break;
            default:
                snprintf(line, sizeof line, "\n#%d ?tag %u", i, (unsigned)p[0]);
                p += 1;
                break;
            }
            at = put(out, at, sizeof out, line);
            i += wide ? 2 : 1;
        }
        (*agent)->Deallocate(agent, bytes);
    }
    (*agent)->GetClassMethods(agent, klass, &mcount, &methods);
    for (k = 0; k < mcount; k++) {
        char *name = NULL;
        char *sig = NULL;
        jint size = 0;
        unsigned char *code = NULL;
        jvmtiError berr;
        (*agent)->GetMethodName(agent, methods[k], &name, &sig, NULL);
        berr = (*agent)->GetBytecodes(agent, methods[k], &size, &code);
        snprintf(line, sizeof line, "\nmethod %s%s code=", name, sig);
        at = put(out, at, sizeof out, line);
        if (berr == JVMTI_ERROR_NONE) {
            int b;
            for (b = 0; b < size; b++) {
                snprintf(line, sizeof line, "%02x", code[b]);
                at = put(out, at, sizeof out, line);
            }
            (*agent)->Deallocate(agent, code);
        } else {
            snprintf(line, sizeof line, "err=%d", (int)berr);
            at = put(out, at, sizeof out, line);
        }
        (*agent)->Deallocate(agent, (unsigned char *)name);
        (*agent)->Deallocate(agent, (unsigned char *)sig);
    }
    if (methods != NULL) {
        (*agent)->Deallocate(agent, (unsigned char *)methods);
    }
    return (*env)->NewStringUTF(env, out);
}
