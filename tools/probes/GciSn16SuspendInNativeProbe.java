// gci wave i16, lane sn: JVMTI `SuspendThread` of a thread whose JNI native's
// C code loops over JNIEnv / JVMTI calls
// (docs/known-issues/gc/gci-i15rb-vm-default-path-changes-review-findings-20261014.md,
// finding 6).
//
// HotSpot blocks a suspended thread at its next native -> VM transition
// (`ThreadInVMfromNative`): once `SuspendThread` returned, the looper's C code
// completes at most the call it is in, then makes no further JNIEnv / JVMTI
// call until `ResumeThread`. CratonVM ran every such call while suspended
// (the thread is in native with `CRATONVM_JNI_NATIVE_TRANSITIONS`, so the
// suspension counted it stopped at once); gci i16/sn holds it at the JNI entry
// behind `CRATONVM_DEBUG_SUSPEND_AT_NATIVE_TRANSITION=1`.
//
// Per kind (0: GetObjectClass, 1: GetArrayLength, 2: JVMTI GetTag, 3: JVMTI
// GetThreadState): start a looper in the native, wait until it has made 1000
// calls, SuspendThread it, let 50 ms settle, count its calls over the hold
// window, ResumeThread it, and check it runs on (1000 more calls). With "gc"
// a third thread calls System.gc() back to back for the whole run, and the
// probe checks that collections complete while the looper is held.
//
// Needs a CratonVM built with the vm crate's `experimental-debug` feature (the
// C jvmtiEnv) and the shim tools/probes/jni/GciSn16SuspendInNativeProbe.c,
// loaded as an agent AND with System.load (args[0]) for the natives:
//
//   cl /LD /O1 /I"%JDK%\include" /I"%JDK%\include\win32" GciSn16SuspendInNativeProbe.c /Fegcisn16sus.dll
//   java     -XX:+UseSerialGC -Xmx64m -agentpath:<dll> -cp <classes> GciSn16SuspendInNativeProbe <dll> 300 gc
//   cratonvm -Xmx64m -agentpath:<dll> -cp <classes> GciSn16SuspendInNativeProbe <dll> 300 gc
//
// Expected stdout (HotSpot 25; deterministic):
//
//   init 0
//   kind 0 suspend 0 during 0 resume 0 resumed true rc 0
//   kind 1 suspend 0 during 0 resume 0 resumed true rc 0
//   kind 2 suspend 0 during 0 resume 0 resumed true rc 0
//   kind 3 suspend 0 during 0 resume 0 resumed true rc 0
//   gc progressed true          (only with "gc")
//
// `during` is `0` when the looper made no call in the hold window, `moved`
// otherwise. stderr (diagnostics only): one
// `[sn16] kind=<k> during=<n> hold_ms=<n> warm_ms=<n> state=0x<hex> gcs_during=<n>`
// per kind and `[sn16] gcs=<n>`.
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicLong;

public class GciSn16SuspendInNativeProbe {
    static final AtomicLong GCS = new AtomicLong();
    static final long WAIT_NS = 60_000_000_000L;

    /** 0 with the agent's jvmtiEnv and its capabilities; else negative. */
    static native int init();

    /** Loop one VM entry of `kind` per round until stopped: 0 or a negative code. */
    static native int loop(int kind, Object obj, int[] arr);

    static native void setStop(int v);

    /** The calls every looper completed, in all. */
    static native long calls();

    static native int suspend(Thread t);

    static native int resume(Thread t);

    /** JVMTI thread state of t, masked to SUSPENDED | IN_NATIVE | ALIVE. */
    static native int state(Thread t);

    public static void main(String[] args) throws Exception {
        System.load(args[0]);
        int holdMs = args.length > 1 ? Integer.parseInt(args[1]) : 300;
        boolean gc = args.length > 2 && args[2].equals("gc");
        int init = init();
        System.out.println("init " + init);
        if (init != 0) {
            return;
        }
        AtomicBoolean stopGc = new AtomicBoolean();
        Thread gcLoop = null;
        if (gc) {
            gcLoop = new Thread(() -> {
                while (!stopGc.get()) {
                    System.gc();
                    GCS.incrementAndGet();
                }
            }, "sn16-gc");
            gcLoop.setDaemon(true);
            gcLoop.start();
        }
        boolean gcProgressed = true;
        for (int kind = 0; kind < 4; kind++) {
            long gcsDuring = runKind(kind, holdMs);
            if (gc && gcsDuring <= 0) {
                gcProgressed = false;
            }
        }
        if (gc) {
            stopGc.set(true);
            gcLoop.join();
            System.out.println("gc progressed " + gcProgressed);
        }
        System.err.println("[sn16] gcs=" + GCS.get());
    }

    /** One kind's suspension; answers the collections completed while it was held. */
    static long runKind(int kind, int holdMs) throws Exception {
        setStop(0);
        final Object obj = new Object();
        final int[] arr = new int[4];
        final int[] rc = {Integer.MIN_VALUE};
        long base = calls();
        Thread t = new Thread(() -> rc[0] = loop(kind, obj, arr), "sn16-looper-" + kind);
        t.setDaemon(true);
        long warmStart = System.nanoTime();
        t.start();
        long deadline = System.nanoTime() + WAIT_NS;
        while (calls() - base < 1000 && t.isAlive() && System.nanoTime() < deadline) {
            Thread.sleep(1);
        }
        long warmMs = (System.nanoTime() - warmStart) / 1_000_000;
        int s = suspend(t);
        Thread.sleep(50);
        long c1 = calls();
        long g1 = GCS.get();
        Thread.sleep(holdMs);
        long c2 = calls();
        long g2 = GCS.get();
        int st = state(t);
        int r = resume(t);
        deadline = System.nanoTime() + WAIT_NS;
        while (calls() - c2 < 1000 && t.isAlive() && System.nanoTime() < deadline) {
            Thread.sleep(1);
        }
        boolean resumed = calls() - c2 >= 1000;
        setStop(1);
        t.join(WAIT_NS / 1_000_000);
        long during = c2 - c1;
        System.out.println("kind " + kind + " suspend " + s + " during " + (during == 0 ? "0" : "moved")
                + " resume " + r + " resumed " + resumed + " rc " + rc[0]);
        System.err.println("[sn16] kind=" + kind + " during=" + during + " hold_ms=" + holdMs
                + " warm_ms=" + warmMs + " state=0x" + Integer.toHexString(st)
                + " gcs_during=" + (g2 - g1));
        return g2 - g1;
    }
}
