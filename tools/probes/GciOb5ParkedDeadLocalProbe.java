// GC round gci, wave i5, lane ob (2026-10-12): the decision row for item 1 of
// docs/internal/gc/gch-h1jp-dispatch-doors-and-oome-ladder-review-findings-RETIRED-UNFIXED-20261018.md
// (opt-in CRATONVM_GC_DEPOSIT_LOCALS_LIVENESS).
//
// The question the switch turns on: does a thread PARKED in a blocking native
// keep a scoped-out (liveness-dead) object local of its interpreted caller
// frame alive across a collection? This VM's blocking deposit
// (`NativeContextImpl::deposit_root_snapshot_inner`) roots every local of every
// frame while any thread runs compiled code (the conservative-locals rule);
// the switch makes it keep the bytecode-liveness proof instead. HotSpot's
// interpreter oop maps (`GenerateOopMap`) are type maps, so the expected
// HotSpot answer for a cold (interpreted) frame is `false`: the array stays.
//
// Shape: `holder()` (called once, so interpreted on HotSpot) allocates a 1 MiB
// array, takes its last read, then joins a collector thread. The collector
// waits until `main` is WAITING (parked in `join`, i.e. its blocking deposit is
// published), calls System.gc() twice and ends. A daemon spinner keeps one
// thread in compiled code on a JIT build (the arming condition of the
// conservative-locals rule); it prints nothing.
//
// stdout (HotSpot 25, -XX:+UseSerialGC -Xmx64m), deterministic:
//
//   cleared-while-parked=false
//   done
//
// `true` means the parked frame's dead local was not a root at the pause.
// Nothing timing-dependent is printed. Diagnostics: none.
//
//   javac -d out GciOb5ParkedDeadLocalProbe.java
//   java -XX:+UseSerialGC -Xmx64m -cp out GciOb5ParkedDeadLocalProbe
//   cratonvm --java-home $JDK -XX:+UseGenerationalGC -Xmx64m -cp out GciOb5ParkedDeadLocalProbe
import java.lang.ref.WeakReference;

public class GciOb5ParkedDeadLocalProbe {
    static volatile boolean stop;
    static volatile long sink;
    static WeakReference<byte[]> ref;
    static Thread mainThread;

    public static void main(String[] args) throws Exception {
        mainThread = Thread.currentThread();
        Thread spinner = new Thread(GciOb5ParkedDeadLocalProbe::spin, "ob5-spinner");
        spinner.setDaemon(true);
        spinner.start();
        // Let a JIT build compile the spinner's loop before the deposit.
        Thread.sleep(500);
        boolean cleared = holder();
        stop = true;
        spinner.join();
        System.out.println("cleared-while-parked=" + cleared);
        System.out.println("done");
    }

    static void spin() {
        long s = 1;
        while (!stop) {
            s = step(s);
        }
        sink = s;
    }

    static long step(long s) {
        return s * 31 + 7;
    }

    static boolean holder() throws Exception {
        byte[] dead = new byte[1 << 20];
        dead[0] = 7;
        ref = new WeakReference<>(dead);
        sink += dead.length; // the last read of `dead`
        Thread gc = new Thread(GciOb5ParkedDeadLocalProbe::collect, "ob5-collector");
        gc.start();
        gc.join(); // parked here, `dead` in scope and liveness-dead
        return ref.get() == null;
    }

    static void collect() {
        while (mainThread.getState() != Thread.State.WAITING) {
            Thread.onSpinWait();
        }
        System.gc();
        System.gc();
    }
}
