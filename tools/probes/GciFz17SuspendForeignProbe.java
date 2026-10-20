// gci wave i17, lane fz (i16/sn's residual, docs/internal/gc-round-gci-20261011/i16-sn-report.md
// section 8): JVMTI `SuspendThread` of a host thread that attached with
// `AttachCurrentThread` and loops over JNIEnv calls from its own C code.
//
// HotSpot blocks a suspended thread at its next native -> VM transition
// (`ThreadInVMfromNative`), attached threads included: once `SuspendThread`
// returned, the looper completes at most the call it is in, then makes no
// further JNIEnv call until `ResumeThread`. CratonVM's foreign attachment
// reaches the VM through `ForeignJniEntry::enter_foreign`, which i16/sn did
// not hold; gci i17/fz holds it there behind
// `CRATONVM_DEBUG_SUSPEND_AT_NATIVE_TRANSITION=1`.
//
// Per kind (0: NewStringUTF + DeleteLocalRef, 1: GetArrayLength on a global
// int[]): start a host thread in C that attaches (as a daemon), publishes its
// own java.lang.Thread, and loops; wait for 1000 calls, SuspendThread it, let
// 50 ms settle, count its calls over the hold window, ResumeThread it, and
// check it runs on (1000 more calls). With "gc" a Java thread calls
// System.gc() back to back for the whole run and the probe checks that
// collections complete while the looper is held.
//
// Needs a CratonVM built with the vm crate's `experimental-debug` feature (the
// C jvmtiEnv) and the shim tools/probes/jni/GciFz17SuspendForeignProbe.c,
// loaded as an agent AND with System.load (args[0]) for the natives:
//
//   cl /LD /O1 /I"%JDK%\include" /I"%JDK%\include\win32" GciFz17SuspendForeignProbe.c /Fegcifz17sus.dll
//   java     -XX:+UseSerialGC -Xmx64m -agentpath:<dll> -cp <classes> GciFz17SuspendForeignProbe <dll> 300 gc
//   cratonvm -Xmx64m -agentpath:<dll> -cp <classes> GciFz17SuspendForeignProbe <dll> 300 gc
//
// Expected stdout (HotSpot 25; deterministic):
//
//   init 0
//   kind 0 start 0 suspend 0 during 0 resume 0 resumed true rc 0
//   kind 1 start 0 suspend 0 during 0 resume 0 resumed true rc 0
//   gc progressed true          (only with "gc")
//
// `during` is `0` when the looper made no JNIEnv call in the hold window,
// `moved` otherwise. stderr (diagnostics only): one
// `[fz17] kind=<k> during=<n> hold_ms=<n> warm_ms=<n> state=0x<hex> gcs_during=<n>`
// per kind and `[fz17] gcs=<n>`.
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicLong;

public class GciFz17SuspendForeignProbe {
    static final AtomicLong GCS = new AtomicLong();
    static final long WAIT_NS = 60_000_000_000L;

    /** 0 with the agent's jvmtiEnv and its capabilities; else negative. */
    static native int init();

    /** Start the attached host looper of `kind` on `arr`: 0 or a negative code. */
    static native int start(int kind, int[] arr);

    /** The looper's own java.lang.Thread once it attached, else null. */
    static native Thread looperThread();

    static native void setStop(int v);

    /** 1 once the looper detached; its exit code in `rc()`. */
    static native int done();

    static native int rc();

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
            }, "fz17-gc");
            gcLoop.setDaemon(true);
            gcLoop.start();
        }
        boolean gcProgressed = true;
        for (int kind = 0; kind < 2; kind++) {
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
        System.err.println("[fz17] gcs=" + GCS.get());
    }

    /** One kind's suspension; answers the collections completed while it was held. */
    static long runKind(int kind, int holdMs) throws Exception {
        setStop(0);
        final int[] arr = new int[4];
        long base = calls();
        long warmStart = System.nanoTime();
        int started = start(kind, arr);
        long deadline = System.nanoTime() + WAIT_NS;
        Thread t = null;
        while (started == 0 && (t = looperThread()) == null && System.nanoTime() < deadline) {
            Thread.sleep(1);
        }
        while (started == 0 && calls() - base < 1000 && done() == 0 && System.nanoTime() < deadline) {
            Thread.sleep(1);
        }
        long warmMs = (System.nanoTime() - warmStart) / 1_000_000;
        int s = t == null ? -1 : suspend(t);
        Thread.sleep(50);
        long c1 = calls();
        long g1 = GCS.get();
        Thread.sleep(holdMs);
        long c2 = calls();
        long g2 = GCS.get();
        int st = t == null ? -1 : state(t);
        int r = t == null ? -1 : resume(t);
        deadline = System.nanoTime() + WAIT_NS;
        while (calls() - c2 < 1000 && done() == 0 && System.nanoTime() < deadline) {
            Thread.sleep(1);
        }
        boolean resumed = calls() - c2 >= 1000;
        setStop(1);
        deadline = System.nanoTime() + WAIT_NS;
        while (started == 0 && done() == 0 && System.nanoTime() < deadline) {
            Thread.sleep(1);
        }
        long during = c2 - c1;
        System.out.println("kind " + kind + " start " + started + " suspend " + s + " during "
                + (during == 0 ? "0" : "moved") + " resume " + r + " resumed " + resumed + " rc " + rc());
        System.err.println("[fz17] kind=" + kind + " during=" + during + " hold_ms=" + holdMs
                + " warm_ms=" + warmMs + " state=0x" + Integer.toHexString(st)
                + " gcs_during=" + (g2 - g1));
        return g2 - g1;
    }
}
