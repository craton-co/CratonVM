// Interpreter round i6 wave 121, lane L2 (page
// `i18-L3-proposal-jdwp-lists-the-compiled-activations-of-a-blocked-thread`,
// section "Wave 121 ... the CompiledHome chain index").
//
// A worker warms `holder` until it compiles, then takes a monitor in it and
// blocks in `Thread.sleep` below it. The main thread reads the worker's locked
// monitors (`ThreadMXBean.getThreadInfo(.., true, false)`) and prints the
// method and stack depth each one is attributed to. HotSpot attributes the
// monitor to `holder` at its own depth. Before the fix, with both
// `CRATONVM_JVMTI_C_LISTING_SHARED=1` and
// `CRATONVM_JIT_DEBUGGER_LOCALS_HOME_SPILL=1` and a debugger attached, a
// listing row with home-slot locals lost its chain index and the JVMTI owned-
// monitor depth fell back to -1; this probe is the management-API twin of that
// question and must print the same lines in every configuration.
//
// Run (HotSpot):  java L2W121CompiledHomeMonitorDepth.java
// Run (CratonVM): cratonvm [--jdk-only|--compatible] L2W121CompiledHomeMonitorDepth
//   with and without CRATONVM_JIT_DEBUGGER_LOCALS_HOME_SPILL=1
//   CRATONVM_JVMTI_C_LISTING_SHARED=1, JIT on.
//
// HOTSPOT_EXPECTED_BEGIN
// warm 200000
// monitors 1
// monitor L2W121CompiledHomeMonitorDepth$Lock in holder depth-from-holder 0
// depth-sane true
// done
// HOTSPOT_EXPECTED_END

import java.lang.management.ManagementFactory;
import java.lang.management.MonitorInfo;
import java.lang.management.ThreadInfo;
import java.lang.management.ThreadMXBean;
import java.util.concurrent.CountDownLatch;

public class L2W121CompiledHomeMonitorDepth {
    static final class Lock {}

    static final Lock LOCK = new Lock();
    static final CountDownLatch IN = new CountDownLatch(1);
    static volatile boolean park;
    static long sink;

    static long holder(int i, long acc) {
        synchronized (LOCK) {
            long local = acc + i;
            if (park) {
                IN.countDown();
                try {
                    Thread.sleep(60_000);
                } catch (InterruptedException e) {
                    // released by main
                }
            }
            return local;
        }
    }

    public static void main(String[] args) throws Exception {
        long acc = 0;
        int warm = 200_000;
        for (int i = 0; i < warm; i++) {
            acc = holder(i, acc);
        }
        sink = acc;
        System.out.println("warm " + warm);
        park = true;
        Thread worker = new Thread(() -> sink += holder(7, 35), "w121-holder");
        worker.start();
        IN.await();
        ThreadMXBean mx = ManagementFactory.getThreadMXBean();
        ThreadInfo info = mx.getThreadInfo(new long[] {worker.threadId()}, true, false)[0];
        MonitorInfo[] monitors = info.getLockedMonitors();
        System.out.println("monitors " + monitors.length);
        StackTraceElement[] st = info.getStackTrace();
        int holderDepth = -1;
        for (int i = 0; i < st.length; i++) {
            if (st[i].getMethodName().equals("holder")) {
                holderDepth = i;
                break;
            }
        }
        boolean sane = true;
        for (MonitorInfo m : monitors) {
            StackTraceElement f = m.getLockedStackFrame();
            int depth = m.getLockedStackDepth();
            System.out.println("monitor " + m.getClassName() + " in "
                    + (f == null ? "null" : f.getMethodName())
                    + " depth-from-holder " + (depth - holderDepth));
            sane &= depth >= 0 && holderDepth >= 0;
        }
        System.out.println("depth-sane " + sane);
        worker.interrupt();
        worker.join();
        System.out.println("done");
    }
}
