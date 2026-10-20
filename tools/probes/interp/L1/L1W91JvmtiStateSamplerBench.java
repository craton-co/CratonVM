// Interpreter round i4 wave 91, lane L1 -- bench probe for the default-off
// `CRATONVM_JVMTI_STATE_SUSPENSION_HINT` (interpreter round i3 wave 72, lane
// L6; docs/internal/proposals/interpreter/i64-L1-proposal-suspended-read-from-a-word-every-suspension-writes-20261028.md):
// with it on, the C JVMTI table's `GetThreadState` skips the VM's debug-state
// lock for the `SUSPENDED` bit while no thread is suspended (the profiler
// case). This bench is the sampler the page asks for: 8 threads (4 parked, 4
// sleeping) and `GetThreadState` over all of them in a loop, from one sampler
// thread and from four at once (where the lock contends).
//
// Needs the native shim tools/probes/interp/L1/L1W91JvmtiStateSamplerBench.c,
// loaded both as an agent and with `System.load`:
//
//   gcc -O2 -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w91sampler.so tools/probes/interp/L1/L1W91JvmtiStateSamplerBench.c
//   javac -d /tmp/l1w91sampler tools/probes/interp/L1/L1W91JvmtiStateSamplerBench.java
//   A/B (interleave the arms, 5 reps, medians of the per-run minimums):
//   cratonvm [--compatible] -agentpath:/tmp/libl1w91sampler.so -cp /tmp/l1w91sampler L1W91JvmtiStateSamplerBench /tmp/libl1w91sampler.so
//   CRATONVM_JVMTI_STATE_SUSPENSION_HINT=1 cratonvm [--compatible] -agentpath:... (same arguments)
//   java -agentpath:/tmp/libl1w91sampler.so -cp /tmp/l1w91sampler L1W91JvmtiStateSamplerBench /tmp/libl1w91sampler.so
//
// Rows (stdout `TIME ` lines since wave 116, min ns per GetThreadState call over REPS rounds):
//   sampler-1   one sampler thread;
//   sampler-4   four sampler threads at once (wall time / total calls).
// The lines without "ns/" are checksums (the OR of every state word read:
// PARKED waiting 0x291 | SLEEPING 0xe1), identical on every VM and both arms:
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3 HotSpot, Windows, MSVC-built shim; MEASURED in wave 116: sampler-1 170 ns, sampler-4 973 ns)
// checksum sampler-1=0x2f1 sampler-4=0x2f1
// done
// HOTSPOT_EXPECTED_END
import java.util.concurrent.locks.LockSupport;

public class L1W91JvmtiStateSamplerBench {
    static final int REPS = 7;
    static final int ROUNDS = 20_000;
    static final int SAMPLERS = 4;

    static native int sample(Thread[] threads, int rounds);

    static volatile boolean stop;

    public static void main(String[] args) throws Exception {
        if (args.length < 1) {
            System.out.println("usage: L1W91JvmtiStateSamplerBench <path of the shim, also given as -agentpath>");
            return;
        }
        System.load(args[0]);
        Thread[] targets = new Thread[8];
        for (int i = 0; i < 4; i++) {
            targets[i] = new Thread(() -> {
                while (!stop) {
                    LockSupport.park();
                }
            }, "parked-" + i);
            targets[4 + i] = new Thread(() -> {
                while (!stop) {
                    try {
                        Thread.sleep(600_000);
                    } catch (InterruptedException e) {
                        // re-check stop
                    }
                }
            }, "sleeping-" + i);
        }
        for (Thread t : targets) {
            t.setDaemon(true);
            t.start();
        }
        for (int i = 0; i < 4; i++) {
            while (targets[i].getState() != Thread.State.WAITING
                    || targets[4 + i].getState() != Thread.State.TIMED_WAITING) {
                Thread.sleep(5);
            }
        }
        // Warm-up.
        sample(targets, ROUNDS / 10);

        long best1 = Long.MAX_VALUE;
        int sum1 = 0;
        for (int rep = 0; rep < REPS; rep++) {
            long t0 = System.nanoTime();
            sum1 |= sample(targets, ROUNDS);
            long dt = System.nanoTime() - t0;
            best1 = Math.min(best1, dt / ((long) ROUNDS * targets.length));
        }

        long best4 = Long.MAX_VALUE;
        int[] sums = new int[SAMPLERS];
        for (int rep = 0; rep < REPS; rep++) {
            Thread[] samplers = new Thread[SAMPLERS];
            for (int s = 0; s < SAMPLERS; s++) {
                final int idx = s;
                samplers[s] = new Thread(() -> sums[idx] |= sample(targets, ROUNDS / SAMPLERS));
            }
            long t0 = System.nanoTime();
            for (Thread s : samplers) {
                s.start();
            }
            for (Thread s : samplers) {
                s.join();
            }
            long dt = System.nanoTime() - t0;
            best4 = Math.min(best4, dt / ((long) ROUNDS * targets.length));
        }
        int sum4 = 0;
        for (int s : sums) {
            sum4 |= s;
        }

        System.out.println("TIME sampler-1 ns/call " + best1);
        System.out.println("TIME sampler-4 ns/call " + best4);
        System.out.println("checksum sampler-1=0x" + Integer.toHexString(sum1)
                + " sampler-4=0x" + Integer.toHexString(sum4));
        stop = true;
        for (int i = 0; i < 4; i++) {
            LockSupport.unpark(targets[i]);
            targets[4 + i].interrupt();
        }
        for (Thread t : targets) {
            t.join(10_000);
        }
        System.out.println("done");
    }
}
