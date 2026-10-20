import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.lang.invoke.WrongMethodTypeException;

/**
 * gci i17/na: fill the heap to an OutOfMemoryError, keep it full, and drive a
 * VM door that builds a NativeContext OUTSIDE the native call funnel (where the
 * native allocators used to take their infallible arm and abort the process):
 *
 * <ul>
 *   <li>{@code vh}: an invoke-exact VarHandle accessed at the wrong type. The
 *       interpreter's exact check ({@code invoke::var_handle_exact_refusal})
 *       builds the WrongMethodTypeException in Rust.</li>
 *   <li>{@code jfr}: a contended monitor enter (about 20 ms) under a JFR
 *       recording of {@code jdk.JavaMonitorEnter}: the interpreter's
 *       monitorenter posts the event through its own context
 *       ({@code opcodes::op_monitorenter}).</li>
 * </ul>
 *
 * Stdout is deterministic (counts only, printed after the heap is released);
 * the fill size goes to stderr. On a full heap either an OutOfMemoryError or
 * the door's own exception is an acceptable answer (HotSpot gives one or the
 * other depending on how full the heap is), so both count as "refused".
 */
public class GciNa17NativeDoorsProbe {
    static long field;
    static Object[] chain;
    static volatile int phase;

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "vh";
        System.out.println("mode " + mode);
        if (mode.equals("vh")) {
            vh();
        } else if (mode.equals("jfr")) {
            jfr();
        } else {
            System.out.println("unknown mode");
        }
        System.out.println("done");
    }

    /** Fill the heap until an OutOfMemoryError, twice (large then small chunks). */
    static int fill() {
        Object[] head = null;
        int chunks = 0;
        try {
            while (true) {
                Object[] node = new Object[2];
                node[0] = head;
                node[1] = new long[4096];
                head = node;
                chunks++;
            }
        } catch (OutOfMemoryError e) {
            // keep what was allocated
        }
        chain = head;
        head = null;
        try {
            while (true) {
                Object[] node = new Object[2];
                node[0] = chain;
                chain = node;
                chunks++;
            }
        } catch (OutOfMemoryError e) {
            // the heap is full
        }
        return chunks;
    }

    /** 0 = refused (the door's exception or OOME), 1 = another throwable, 2 = admitted. */
    static int attemptCode(VarHandle vh) {
        try {
            vh.set(1);
            return 2;
        } catch (WrongMethodTypeException | OutOfMemoryError e) {
            return 0;
        } catch (Throwable t) {
            return 1;
        }
    }

    static String attemptName(VarHandle vh) {
        try {
            vh.set(1);
            return "admitted";
        } catch (Throwable t) {
            return t.getClass().getName();
        }
    }

    static void vh() throws Exception {
        VarHandle vh = MethodHandles.lookup()
                .findStaticVarHandle(GciNa17NativeDoorsProbe.class, "field", long.class)
                .withInvokeExactBehavior();
        String before = attemptName(vh);
        int chunks = fill();
        int refused = 0;
        int other = 0;
        int admitted = 0;
        for (int i = 0; i < 200; i++) {
            int code = attemptCode(vh);
            if (code == 0) {
                refused++;
            } else if (code == 1) {
                other++;
            } else {
                admitted++;
            }
        }
        chain = null;
        System.gc();
        String after = attemptName(vh);
        System.out.println("vh before: " + before);
        System.out.println("vh full: refused " + refused + " other " + other + " admitted " + admitted);
        System.out.println("vh after: " + after);
        System.err.println("[na17] vh chunks=" + chunks);
    }

    static void jfr() throws Exception {
        final Object lock = new Object();
        final int rounds = 5;
        Thread holder = new Thread(() -> {
            for (int round = 0; round < rounds; round++) {
                while (phase != 2 * round + 1) {
                    Thread.onSpinWait();
                }
                synchronized (lock) {
                    phase = 2 * round + 2;
                    long end = System.nanoTime() + 20_000_000L;
                    while (System.nanoTime() < end) {
                        Thread.onSpinWait();
                    }
                }
            }
        }, "na17-holder");
        holder.setDaemon(true);
        holder.start();
        jdk.jfr.Recording rec = new jdk.jfr.Recording();
        rec.enable("jdk.JavaMonitorEnter").withThreshold(java.time.Duration.ZERO);
        rec.start();
        int chunks = fill();
        int entered = 0;
        for (int round = 0; round < rounds; round++) {
            phase = 2 * round + 1;
            while (phase != 2 * round + 2) {
                Thread.onSpinWait();
            }
            synchronized (lock) {
                entered++;
            }
        }
        chain = null;
        System.gc();
        rec.stop();
        rec.close();
        holder.join();
        System.out.println("jfr full: entered " + entered);
        System.err.println("[na17] jfr chunks=" + chunks);
    }
}
