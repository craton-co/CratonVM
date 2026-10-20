// Interpreter round i2 wave 63, lane L1: whether the platform thread a
// mounted virtual thread runs on is a Java thread, the residual of the C
// JVMTI table's `GetCarrierThread` / `GetVirtualThread`
// (docs/internal/fixed-bugs/interpreter-i63-L1-a-virtual-threads-carrier-is-not-a-java-thread-FIXED-20261010.md).
//
// Rows, while a virtual thread spins mounted:
//
// * `carriers listed`: `Thread.getAllStackTraces()` holds a thread named
//   `ForkJoinPool-1-worker-<n>` (the JDK's default scheduler's carrier);
// * `vt listed`: it holds the virtual thread (HotSpot lists platform
//   threads only).
//
//   javac -d /tmp/l1w63car tools/probes/interp/L1/L1W63CarrierThreadsListed.java
//   java     -cp /tmp/l1w63car L1W63CarrierThreadsListed
//   cratonvm -cp /tmp/l1w63car L1W63CarrierThreadsListed
//
// Expected stdout (HotSpot 25.0.3): MEASURED on the Windows box, three runs,
// the same each time:
//
//   carriers listed: true
//   vt listed: false
//
// CratonVM (wave 63, both modes): `carriers listed: false`; `vt listed` was
// `true` until wave 63's `Thread.getThreads` filter (now `false`). Since
// 2026-10-10 (JDK-driven virtual threads, `vm/src/vm/jdk_continuation.rs`)
// both rows equal HotSpot's in both modes; `CRATONVM_VT_CONTINUATIONS=0`
// (the bound shape) prints `carriers listed: false` again.
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.atomic.AtomicBoolean;

public class L1W63CarrierThreadsListed {
    public static void main(String[] args) throws Exception {
        AtomicBoolean stop = new AtomicBoolean();
        CountDownLatch up = new CountDownLatch(1);
        Thread vt = Thread.ofVirtual().start(() -> {
            up.countDown();
            while (!stop.get()) {
                Thread.onSpinWait();
            }
        });
        up.await();
        long carriers = Thread.getAllStackTraces().keySet().stream()
            .filter(t -> t.getName().startsWith("ForkJoinPool-1-worker"))
            .count();
        System.out.println("carriers listed: " + (carriers > 0));
        System.out.println("vt listed: " + Thread.getAllStackTraces().containsKey(vt));
        stop.set(true);
        vt.join();
    }
}
