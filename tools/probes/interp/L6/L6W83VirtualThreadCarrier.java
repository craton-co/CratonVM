// Interpreter round i3 wave 83, lane L6: which threads a started virtual
// thread brings with it, and what kind of thread it is -- the one design root
// of docs/internal/fixed-bugs/interpreter-i63-L1-a-virtual-threads-carrier-is-not-a-java-thread-FIXED-20261010.md
// and docs/internal/fixed-bugs/interpreter-i65-L1-virtual-thread-unblocker-reads-timed-waiting-FIXED-20261010.md
// (`ContinuationSupport.isSupported0()` answers false here, so every virtual
// thread is a `ThreadBuilders$BoundVirtualThread`: no `java.lang.VirtualThread`,
// no default `ForkJoinPool` scheduler, no carriers, no unblocker).
//
// Rows, while one virtual thread is parked in `CountDownLatch.await`:
//
// * `vt class`: the virtual thread's class (HotSpot: `java.lang.VirtualThread`);
// * `vt state`: WAITING on both;
// * `vt listed`: whether `Thread.getAllStackTraces()` holds it (HotSpot: no,
//   platform threads only; fixed here in wave 63);
// * `unblocker`: the `VirtualThread-unblocker` thread `VirtualThread.<clinit>`
//   starts (JDK 25 `VirtualThread.java`, its last static initialiser:
//   `InnocuousThread.newThread("VirtualThread-unblocker", ..)`, daemon), with
//   its class, state and daemon flag; HotSpot's `takeVirtualThreadListToUnblock`
//   waits inside the VM, so RUNNABLE (measured by `L1W65ServedWaitStates`);
// * `carriers listed` / `carrier class`: a thread named
//   `ForkJoinPool-<n>-worker-<m>` (the default scheduler's worker; the pool
//   number is masked) and its class (JDK 25 `VirtualThread.createDefaultScheduler`:
//   `pool -> new CarrierThread(pool)`); the worker stays alive after the
//   virtual thread parks (the scheduler's keep-alive is 30 s);
// * `after join`: TERMINATED.
//
//   javac -d /tmp/l6w83vtc tools/probes/interp/L6/L6W83VirtualThreadCarrier.java
//   java     -cp /tmp/l6w83vtc L6W83VirtualThreadCarrier
//   cratonvm -cp /tmp/l6w83vtc L6W83VirtualThreadCarrier
//   cratonvm --compatible -cp /tmp/l6w83vtc L6W83VirtualThreadCarrier
//
// The block below is derived from the JDK 25 sources and the two measured
// rows (`unblocker` RUNNABLE, `carriers listed: true`) of L1W65 / L1W63.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// vt class: java.lang.VirtualThread
// vt state: WAITING
// vt listed: false
// unblocker: jdk.internal.misc.InnocuousThread RUNNABLE daemon=true
// carriers listed: true
// carrier class: jdk.internal.misc.CarrierThread
// after join: TERMINATED
// HOTSPOT_EXPECTED_END
//
// CratonVM (wave 83, both modes, by reading; not changed this wave):
// `vt class: java.lang.ThreadBuilders$BoundVirtualThread`, `unblocker: absent`,
// `carriers listed: false`, `carrier class: none`; the other rows match.
//
// CratonVM since 2026-10-10 (JDK-driven virtual threads,
// `vm/src/vm/jdk_continuation.rs`; MEASURED on the host, JDK 25.0.4 and
// 21.0.12, both modes, JIT and `--nojit`): every row equals the HotSpot block.
// `CRATONVM_VT_CONTINUATIONS=0` restores the wave-83 rows above.
import java.util.Map;
import java.util.concurrent.CountDownLatch;

public class L6W83VirtualThreadCarrier {
    public static void main(String[] args) throws Exception {
        CountDownLatch inside = new CountDownLatch(1);
        CountDownLatch release = new CountDownLatch(1);
        Thread vt = Thread.ofVirtual().name("l6w83-vt").start(() -> {
            inside.countDown();
            try {
                release.await();
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
            }
        });
        inside.await();
        long deadline = System.nanoTime() + 10_000_000_000L;
        while (vt.getState() != Thread.State.WAITING && System.nanoTime() < deadline) {
            Thread.sleep(5);
        }
        System.out.println("vt class: " + vt.getClass().getName());
        System.out.println("vt state: " + vt.getState());

        Map<Thread, StackTraceElement[]> all = Thread.getAllStackTraces();
        System.out.println("vt listed: " + all.containsKey(vt));

        Thread unblocker = null;
        Thread carrier = null;
        for (Thread t : all.keySet()) {
            String name = t.getName();
            if (name.equals("VirtualThread-unblocker")) {
                unblocker = t;
            } else if (carrier == null && name.matches("ForkJoinPool-\\d+-worker-\\d+")) {
                carrier = t;
            }
        }
        if (unblocker == null) {
            System.out.println("unblocker: absent");
        } else {
            // The unblocker loops back into its wait at once; give a reading
            // taken mid-hand-off a few chances to settle.
            Thread.State s = unblocker.getState();
            for (int i = 0; i < 50 && s != Thread.State.RUNNABLE; i++) {
                Thread.sleep(2);
                s = unblocker.getState();
            }
            System.out.println("unblocker: " + unblocker.getClass().getName() + " " + s
                + " daemon=" + unblocker.isDaemon());
        }
        System.out.println("carriers listed: " + (carrier != null));
        System.out.println("carrier class: " + (carrier == null ? "none" : carrier.getClass().getName()));

        release.countDown();
        vt.join();
        System.out.println("after join: " + vt.getState());
    }
}
