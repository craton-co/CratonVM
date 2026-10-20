// gci wave i15, lane hx: a JNI native that makes thousands of JNIEnv calls per
// invocation while another thread asks for full collections back to back and
// a third churns the young generation
// (docs/known-issues/gc/gci-i15hx-jvmti-tag-agent-hangs-with-native-transitions-20261014.md).
//
// With the JNI native transitions (CRATONVM_JNI_NATIVE_TRANSITIONS, default ON)
// the native's C code is excluded from every pause and EVERY JNIEnv call it
// makes leaves native, waiting out a pause that is merely requested. The
// gcloop thread raised the next pause before the woken caller got back in, so
// each JNIEnv call cost about one whole collection: `GciHj9TagUnderGcProbe`
// (JVMTI tag functions, ~8 000 calls per round) ran past 600 s where the
// package off took 5 s. This probe is the same shape with plain JNI only (no
// agent, no experimental-debug build): `sumAll` reads every element of a
// 1000-element array through GetObjectArrayElement + GetIntField +
// DeleteLocalRef (3 001 JNIEnv calls per invocation).
//
//   cl /LD /O1 /I"%JDK%\include" /I"%JDK%\include\win32" GciHx15JniCallsUnderGcLoopProbe.c /Fegcihx15jni.dll
//   java     -XX:+UseSerialGC -Xmx64m -cp <classes> GciHx15JniCallsUnderGcLoopProbe <dll> 100
//   cratonvm -Xmx64m -cp <classes> GciHx15JniCallsUnderGcLoopProbe <dll> 100
//
// Expected stdout (HotSpot 25; deterministic):
//
//   init 0
//   rounds 100 bad 0 total 49950000
//
// stderr (diagnostics only): `[hx15j] gcs=<System.gc calls> ms=<main loop ms>`.
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicLong;

public class GciHx15JniCallsUnderGcLoopProbe {
    public static class Node {
        public int value;

        public Node(int value) {
            this.value = value;
        }
    }

    static final int N = 1000;

    /** Caches `Node.value`'s field id; 0, or -2 when it is missing. */
    static native int init(Class<?> nodeClass);

    /** The sum of `objs[i].value`, read through JNI one element at a time. */
    static native long sumAll(Object[] objs);

    static final Object[] RING = new Object[256];

    public static void main(String[] args) throws Exception {
        System.load(args[0]);
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 100;
        System.out.println("init " + init(Node.class));
        Object[] objs = new Object[N];
        long expected = 0;
        for (int i = 0; i < N; i++) {
            objs[i] = new Node(i);
            expected += i;
        }

        AtomicBoolean stop = new AtomicBoolean();
        AtomicLong gcs = new AtomicLong();
        Thread gc = new Thread(() -> {
            while (!stop.get()) {
                System.gc();
                gcs.incrementAndGet();
            }
        }, "gcloop");
        Thread alloc = new Thread(() -> {
            int k = 0;
            while (!stop.get()) {
                RING[k & 255] = new byte[512];
                k++;
            }
        }, "alloc");
        gc.setDaemon(true);
        alloc.setDaemon(true);
        gc.start();
        alloc.start();

        long start = System.nanoTime();
        int bad = 0;
        long total = 0;
        for (int r = 1; r <= rounds; r++) {
            long s = sumAll(objs);
            if (s != expected) {
                bad++;
            }
            total += s;
        }
        long ms = (System.nanoTime() - start) / 1_000_000;
        stop.set(true);
        gc.join();
        alloc.join();
        System.out.println("rounds " + rounds + " bad " + bad + " total " + total);
        System.err.println("[hx15j] gcs=" + gcs.get() + " ms=" + ms);
    }
}
