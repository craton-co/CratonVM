// Interpreter round i2 wave 61, lane L6: whole-VM stack dumps of many running
// threads (`Thread.getAllStackTraces()`, `ThreadMXBean.dumpAllThreads`,
// `getThreadInfo(ids, true, false, 3)`), which CratonVM serves with one
// frame-trace pause for all running targets
// (docs/internal/fixed-bugs/interpreter-L6-proposal-one-frame-trace-pause-for-a-whole-thread-dump-FIXED-20261025.md,
// `CRATONVM_THREAD_DUMP_ONE_PAUSE`).
//
// Eight daemon threads `spin-<k>` spin in Java inside `spin<k % 2>` called
// from `loop`; one sleeps. Each dump door prints, per thread in name order,
// its state and its top two frames. Three dumps per door (the first-call
// rows again).
//
// Expected output (HotSpot 25.0.3, default flags, the Windows box; three
// runs, the same each time):
//
//   all sleeper: TIMED_WAITING sleepNanos0 < sleepNanos
//   all spin-0: RUNNABLE spin0 < loop
//   all spin-1: RUNNABLE spin1 < loop
//   ... (spin-2 .. spin-7 alternating spin0 / spin1)
//   jmx sleeper: TIMED_WAITING sleepNanos0 < sleepNanos
//   ... (the same nine rows with `jmx`, then with `ids`)
//   (the 27 rows twice more)
//   done
//
// On CratonVM, `CRATONVM_FRAME_TRACE=1` prints one `[THREAD_DUMP_ONE_PAUSE]`
// line per dump (the positive control), and `--verbose:gc` one
// `door=frame-trace` pause per dump instead of one per running thread.
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadInfo;
import java.lang.management.ThreadMXBean;
import java.util.Map;
import java.util.TreeMap;

public class L6W61OneDumpPause {
    static final java.util.concurrent.atomic.AtomicLong started = new java.util.concurrent.atomic.AtomicLong();

    static void spin0() {
        long x = 0;
        started.incrementAndGet();
        while (true) {
            x++;
            if (x == Long.MIN_VALUE) {
                System.out.println(x);
            }
        }
    }

    static void spin1() {
        long x = 0;
        started.incrementAndGet();
        while (true) {
            x += 3;
            if (x == Long.MIN_VALUE) {
                System.out.println(x);
            }
        }
    }

    static void loop(long k) {
        if (k % 2 == 0) {
            spin0();
        } else {
            spin1();
        }
    }

    static String top2(StackTraceElement[] st) {
        if (st.length == 0) {
            return "<empty>";
        }
        return st[0].getMethodName() + (st.length > 1 ? " < " + st[1].getMethodName() : "");
    }

    static boolean ours(String name) {
        return name.startsWith("spin-") || name.equals("sleeper");
    }

    static void dumps(ThreadMXBean mx, long[] ids) {
        TreeMap<String, String> all = new TreeMap<>();
        for (Map.Entry<Thread, StackTraceElement[]> e : Thread.getAllStackTraces().entrySet()) {
            if (ours(e.getKey().getName())) {
                all.put(e.getKey().getName(), e.getKey().getState() + " " + top2(e.getValue()));
            }
        }
        all.forEach((k, v) -> System.out.println("all " + k + ": " + v));
        TreeMap<String, String> jmx = new TreeMap<>();
        for (ThreadInfo ti : mx.dumpAllThreads(false, false)) {
            if (ours(ti.getThreadName())) {
                jmx.put(ti.getThreadName(), ti.getThreadState() + " " + top2(ti.getStackTrace()));
            }
        }
        jmx.forEach((k, v) -> System.out.println("jmx " + k + ": " + v));
        TreeMap<String, String> byIds = new TreeMap<>();
        for (ThreadInfo ti : mx.getThreadInfo(ids, true, false, 3)) {
            byIds.put(ti.getThreadName(), ti.getThreadState() + " " + top2(ti.getStackTrace()));
        }
        byIds.forEach((k, v) -> System.out.println("ids " + k + ": " + v));
    }

    public static void main(String[] args) throws Exception {
        long[] ids = new long[9];
        for (int i = 0; i < 8; i++) {
            final long k = i;
            Thread t = new Thread(() -> loop(k), "spin-" + i);
            t.setDaemon(true);
            t.start();
            ids[i] = t.threadId();
        }
        Thread sleeper = new Thread(() -> {
            try {
                Thread.sleep(1_000_000L);
            } catch (InterruptedException e) {
                // never
            }
        }, "sleeper");
        sleeper.setDaemon(true);
        sleeper.start();
        ids[8] = sleeper.threadId();
        while (started.get() < 8 || sleeper.getState() != Thread.State.TIMED_WAITING) {
            Thread.sleep(1);
        }
        Thread.sleep(300);
        ThreadMXBean mx = ManagementFactory.getThreadMXBean();
        for (int r = 0; r < 3; r++) {
            dumps(mx, ids);
        }
        System.out.println("done");
    }
}
