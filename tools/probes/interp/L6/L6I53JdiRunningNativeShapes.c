// The JNI native of L6I53JdiRunningNativeShapes's `jni` mode: `callBack`
// calls the probe's `once()` back, so the worker runs `Deflater.deflate`
// with a JNI native in the middle of its stack.
//
//   gcc -shared -fPIC -I$JDK/include -I$JDK/include/linux \
//       -o libl6i53jni.so L6I53JdiRunningNativeShapes.c
//   java -Dl6i53.dir=. -cp out L6I53JdiRunningNativeShapes jni
#include <jni.h>
#include <time.h>

JNIEXPORT void JNICALL Java_L6I53JdiRunningNativeShapes_callBack(JNIEnv *env, jclass cls) {
    jmethodID once = (*env)->GetStaticMethodID(env, cls, "once", "()V");
    if (once != NULL) {
        (*env)->CallStaticVoidMethod(env, cls, once);
    }
}

// `jnispin`: busy in C for about `millis` ms, calling no JNI function.
JNIEXPORT void JNICALL Java_L6I53JdiRunningNativeShapes_spin(JNIEnv *env, jclass cls, jint millis) {
    struct timespec start, now;
    volatile unsigned long sink = 0;
    clock_gettime(CLOCK_MONOTONIC, &start);
    do {
        for (int i = 0; i < 100000; i++) {
            sink += (unsigned long) i;
        }
        clock_gettime(CLOCK_MONOTONIC, &now);
    } while ((now.tv_sec - start.tv_sec) * 1000L + (now.tv_nsec - start.tv_nsec) / 1000000L < millis);
}
