// Interpreter round i1 wave 17, lane L2 — the cost of the running-native
// record on the native-call funnel (`vm_exec::safe_native_call_impl`).
//
// Wave 17 records the native a thread runs (`JvmThread::running_native`)
// around every non-leaf native call, but only while a debugger is attached;
// unattached, the funnel pays one load of the session gate and a not-taken
// branch per call. This loop calls a few non-leaf natives (reflection and
// `Thread.holdsLock`) and should time the same as the wave-16 build, within
// noise. Interleave the two builds and compare medians, with and without
// `--nojit`.
//
// stdout is a deterministic checksum (HotSpot 25 prints the same line);
// timings go to stderr.
public class L2W17NativeFunnelBench {
    public static void main(String[] args) throws Exception {
        int rounds = 5;
        int iterations = 400_000;
        int[] array = new int[7];
        Object lock = new Object();
        Class<?> type = String.class;
        long checksum = 0;
        for (int round = 0; round < rounds; round++) {
            long start = System.nanoTime();
            long sum = 0;
            for (int i = 0; i < iterations; i++) {
                sum += java.lang.reflect.Array.getLength(array);
                sum += type.getModifiers();
                sum += Thread.holdsLock(lock) ? 1 : 0;
                sum += type.isInterface() ? 1 : 0;
            }
            long elapsed = System.nanoTime() - start;
            System.err.println("round " + round + ": " + (elapsed / 1_000_000) + " ms, "
                    + (elapsed / (iterations * 4L)) + " ns/call");
            checksum += sum;
        }
        System.out.println("checksum " + checksum);
    }
}
