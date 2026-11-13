// Interpreter round i7 wave 139, lane L1 (taken over from
// docs/feature-designs/jvmti-delivery-threading.md):
// Java that a C agent's JVMTI event callback calls through JNI, on a virtual
// thread, parks. On HotSpot the callback is a native frame, so the park is
// pinned (the virtual thread blocks its carrier) and the upcall returns to
// the callback only after the park; the marker's body runs after that.
//
// 64 virtual threads each call `marker(i)` 5 times while the agent is armed.
// Each call's MethodEntry callback calls `inCallback()`, which parks 1 ms and
// sets a thread-local flag; `marker` checks and clears it (`ordered` counts
// the calls whose callback finished first). Rows:
//
//   hits:    the callbacks that matched the marker (320);
//   ordered: the marker calls that ran after their callback's Java finished
//            (320);
//   parked:  the parks that completed in the callback (320);
//   sum:     the markers' answers (sum over 64 threads of sum (i+1), i in 0..4 = 64 * 15).
//
// CratonVM before wave 139 (predicted from the code, not run): a callback's
// park on a virtual thread yields through the agent's C frames (no native was
// counted for the callback), so the upcall "returns" early: `ordered` and
// `parked` fall short, or the remount runs the upcall's frames into the
// marker's unstarted frame (wrong `sum`, a crash). With
// CRATONVM_JVMTI_CALLBACK_PINS_CONTINUATION=1 (the default since wave 139)
// the park is pinned: HotSpot's block. `=0` is the negative control.
//
// Needs the agent tools/probes/interp/L1/L1W139JvmtiCallbackParkOnVirtualThread.c,
// loaded as an agent and with System.load. Windows (MSVC, from a vcvars64 shell):
//
//   cl /nologo /O2 /LD /I"%JAVA_HOME%\include" /I"%JAVA_HOME%\include\win32" tools\probes\interp\L1\L1W139JvmtiCallbackParkOnVirtualThread.c /Fe:%TEMP%\L1W139.dll
//   javac -d %TEMP%\l1w139 tools\probes\interp\L1\L1W139JvmtiCallbackParkOnVirtualThread.java
//   java -agentpath:%TEMP%\L1W139.dll -cp %TEMP%\l1w139 L1W139JvmtiCallbackParkOnVirtualThread %TEMP%\L1W139.dll
//   cratonvm [--compatible] [--nojit] -agentpath:%TEMP%\L1W139.dll -cp %TEMP%\l1w139 L1W139JvmtiCallbackParkOnVirtualThread %TEMP%\L1W139.dll
//
// HotSpot's stderr carries the System.load native-access warning; compare stdout.
//
// MEASURED on HotSpot 25.0.3 (Temurin 25.0.3+9, Windows, the agent built with
// MSVC 2022 /O2), default flags twice and -Xint, identical:
//
// HOTSPOT_EXPECTED_BEGIN
// arm=0
// hits=320
// ordered=320
// parked=320
// sum=960
// done
// HOTSPOT_EXPECTED_END
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.locks.LockSupport;

public class L1W139JvmtiCallbackParkOnVirtualThread {
    static native int arm();

    static native int disarm();

    static final ThreadLocal<Boolean> CALLED_BACK = ThreadLocal.withInitial(() -> Boolean.FALSE);
    static final AtomicInteger ORDERED = new AtomicInteger();
    static final AtomicInteger PARKED = new AtomicInteger();
    static final AtomicInteger SUM = new AtomicInteger();

    /** Called by the agent's MethodEntry callback, through JNI. */
    static void inCallback() {
        LockSupport.parkNanos(1_000_000);
        PARKED.incrementAndGet();
        CALLED_BACK.set(Boolean.TRUE);
    }

    static int marker(int i) {
        if (CALLED_BACK.get()) {
            ORDERED.incrementAndGet();
        }
        CALLED_BACK.set(Boolean.FALSE);
        return i + 1;
    }

    public static void main(String[] args) throws Exception {
        if (args.length != 1) {
            System.out.println("usage: L1W139JvmtiCallbackParkOnVirtualThread <path of the agent library>");
            return;
        }
        System.load(args[0]);
        // Load everything the run touches before arming.
        marker(0);
        CALLED_BACK.set(Boolean.FALSE);
        Thread warm = Thread.ofVirtual().start(() -> LockSupport.parkNanos(1_000));
        warm.join();
        int arm = arm();
        Thread[] threads = new Thread[64];
        for (int t = 0; t < threads.length; t++) {
            threads[t] = Thread.ofVirtual().start(() -> {
                int s = 0;
                for (int i = 0; i < 5; i++) {
                    s += marker(i);
                }
                SUM.addAndGet(s);
            });
        }
        for (Thread t : threads) {
            t.join(60_000);
        }
        int hits = disarm();
        System.out.println("arm=" + arm);
        System.out.println("hits=" + hits);
        System.out.println("ordered=" + ORDERED.get());
        System.out.println("parked=" + PARKED.get());
        System.out.println("sum=" + SUM.get());
        System.out.println("done");
    }
}
