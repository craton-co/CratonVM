import java.lang.ref.PhantomReference;
import java.lang.ref.Reference;
import java.lang.ref.ReferenceQueue;
import java.lang.ref.WeakReference;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.util.Date;
import java.util.Optional;
import java.util.concurrent.locks.ReentrantLock;

/**
 * Round nio20 wave 6, lane L4: the JIT-throughput rows of {@code R16Fieldsite2Layout} that are not
 * {@code java.nio} ({@code ite}, {@code rlock}, {@code optional}, {@code weakref}, {@code date}),
 * plus the {@code Nio20FenceRow own} shape and two constant-mask table loops, as single small ops
 * so each row's per-op cost can be read on its own.
 *
 * <ul>
 *   <li>{@code ite}       -- two {@code Method.invoke} calls of a method that throws, each caught
 *       as an {@code InvocationTargetException} (four throws per op, as the R16 row).</li>
 *   <li>{@code throwlocal} -- {@code throw new IllegalStateException} caught in the SAME method:
 *       the throw-into-a-handler-of-the-same-compiled-frame floor.</li>
 *   <li>{@code throwcall} -- a callee throws, the caller catches: the unwind floor.</li>
 *   <li>{@code rlock}     -- {@code ReentrantLock.lock(); unlock()} (AQS CAS, {@code
 *       Thread.currentThread()}, {@code exclusiveOwnerThread}).</li>
 *   <li>{@code optional}  -- {@code Optional.ofNullable(..).map(..).filter(..).orElse(..)}.</li>
 *   <li>{@code weakref}   -- {@code new WeakReference(o)}, {@code get()}, {@code refersTo(o)}.</li>
 *   <li>{@code refget}    -- {@code get()} of one long-lived {@code WeakReference} (no
 *       allocation): the {@code Reference.get} floor alone.</li>
 *   <li>{@code date}      -- {@code new Date(t)}, {@code getTime()}, {@code compareTo}, {@code
 *       hashCode}.</li>
 *   <li>{@code fenceown}  -- {@code Nio20FenceRow own}: {@code PLAIN[(i + k) & 7]} over a {@code
 *       static final} table in a 16-trip loop with {@code Reference.reachabilityFence}.</li>
 *   <li>{@code hexmask}   -- {@code HEX[(b >>> 4) & 0xf]}, {@code HEX[b & 0xf]} over a {@code
 *       static final char[]}, 16 bytes per op (the constant-mask predicate's shape).</li>
 *   <li>{@code crcmask}   -- {@code crc = T[(crc ^ b) & 0xff] ^ (crc >>> 8)} over a {@code static
 *       final int[256]}, 16 bytes per op.</li>
 * </ul>
 *
 * <pre>
 *   javac -d /tmp/p20 tools/probes/Nio20JitRows.java
 *   java     -cp /tmp/p20 Nio20JitRows                      # HotSpot reference
 *   cratonvm -cp /tmp/p20 Nio20JitRows                      # --jdk-only (launcher default)
 *   cratonvm --compatible -cp /tmp/p20 Nio20JitRows
 *   # wave 6 L4 A/B (fenceown, hexmask, crcmask move; the rest must not):
 *   CRATONVM_JIT_IR_LOOP_STATIC_HOIST=0 cratonvm -cp /tmp/p20 Nio20JitRows
 *   CRATONVM_JIT_IR_MASK_PREDICATE=0 CRATONVM_JIT_IR_BCE_CONST_MASK=0 cratonvm -cp /tmp/p20 Nio20JitRows
 *   CRATONVM_DBG_LICM=1 cratonvm -Dnio20.rows=fenceown,hexmask,crcmask -cp /tmp/p20 Nio20JitRows 2>&amp;1 \
 *       | grep -E 'static read|mask predicate'
 * </pre>
 *
 * {@code -Dnio20.rows=a,b} selects rows (default all), {@code -Dnio20.n=} the base ops per timed
 * run (default 200 000; {@code ite} / {@code throw*} run N/20). Output: the {@code conf} lines
 * (deterministic, diff them against HotSpot), one {@code check} line (identical in every VM and
 * arm), then one {@code <row> <N> ns/op} line per row.
 */
public class Nio20JitRows {
    static final String ROWS = System.getProperty("nio20.rows", "all");
    static final int N = Integer.getInteger("nio20.n", 200_000);

    static final Object[] PLAIN = {
        "s", Integer.valueOf(1), Long.valueOf(2L), new StringBuilder("x"),
        new Object(), new int[0], Boolean.TRUE, Object.class
    };
    static final char[] HEX = "0123456789abcdef".toCharArray();
    static final int[] CRC = new int[256];

    static {
        for (int n = 0; n < 256; n++) {
            int c = n;
            for (int k = 0; k < 8; k++) {
                c = (c & 1) != 0 ? 0xEDB88320 ^ (c >>> 1) : c >>> 1;
            }
            CRC[n] = c;
        }
    }

    static boolean on(String r) {
        if (ROWS.equals("all")) {
            return true;
        }
        for (String s : ROWS.split(",")) {
            if (s.equals(r)) {
                return true;
            }
        }
        return false;
    }

    // ---- the ops ----

    public static int thrower(int i) {
        throw new IllegalStateException("boom" + (i & 1));
    }

    static final Method THROWER;

    static {
        try {
            THROWER = Nio20JitRows.class.getMethod("thrower", int.class);
        } catch (NoSuchMethodException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    static int iteOp(int i) {
        int h = 0;
        for (int r = 0; r < 2; r++) {
            try {
                THROWER.invoke(null, i + r);
            } catch (InvocationTargetException e) {
                h += e.getCause().getMessage().length() + r;
            } catch (IllegalAccessException e) {
                h -= 1000;
            }
        }
        return h;
    }

    static int throwLocalOp(int i) {
        try {
            if (i >= 0) {
                throw new IllegalStateException("local");
            }
            return 0;
        } catch (IllegalStateException e) {
            return 1 + (i & 3);
        }
    }

    static int throwCallOp(int i) {
        try {
            return thrower(i);
        } catch (IllegalStateException e) {
            return 2 + (i & 3);
        }
    }

    static final ReentrantLock LOCK = new ReentrantLock();
    static int lockCounter;

    static int rlockOp(int i) {
        LOCK.lock();
        try {
            lockCounter += i & 1;
            return lockCounter & 7;
        } finally {
            LOCK.unlock();
        }
    }

    static final Integer[] BOXES = new Integer[64];

    static {
        for (int k = 0; k < BOXES.length; k++) {
            BOXES[k] = (k % 5 == 0) ? null : Integer.valueOf(k);
        }
    }

    static int optionalOp(int i) {
        return Optional.ofNullable(BOXES[i & 63])
                .map(x -> x + 1)
                .filter(x -> (x & 1) == 0)
                .orElse(-1);
    }

    static final Object TARGET = new Object();
    static final WeakReference<Object> LONG_LIVED = new WeakReference<>(TARGET);

    static int weakrefOp(int i) {
        Object o = PLAIN[i & 7];
        WeakReference<Object> w = new WeakReference<>(o);
        int h = w.get() == o ? 1 : 0;
        h += w.refersTo(o) ? 2 : 0;
        Reference.reachabilityFence(o);
        return h;
    }

    static int refGetOp(int i) {
        return LONG_LIVED.get() == TARGET ? (i & 1) : 100;
    }

    static int dateOp(int i) {
        Date d = new Date(1_234_567_890_123L + i);
        Date e = new Date(1_234_567_890_000L);
        return (int) (d.getTime() & 0xff) + d.compareTo(e) + (d.hashCode() & 1);
    }

    static int fenceRow(int i) {
        int h = 0;
        for (int k = 0; k < 16; k++) {
            Object o = PLAIN[(i + k) & 7];
            h += k ^ i;
            Reference.reachabilityFence(o);
        }
        return h;
    }

    static int hexOp(int i) {
        int h = 0;
        for (int k = 0; k < 16; k++) {
            int b = (i * 31 + k * 7) & 0xff;
            h = h * 31 + HEX[(b >>> 4) & 0xf];
            h = h * 31 + HEX[b & 0xf];
        }
        return h;
    }

    static int crcOp(int i) {
        int crc = ~0;
        for (int k = 0; k < 16; k++) {
            int b = (i + k * 13) & 0xff;
            crc = CRC[(crc ^ b) & 0xff] ^ (crc >>> 8);
        }
        return ~crc;
    }

    // ---- the conformance lines ----

    static void conf(StringBuilder out) {
        // ite: the cause, the wrapper's own message, the top frame of the cause.
        try {
            THROWER.invoke(null, 3);
            out.append("conf ite no-throw\n");
        } catch (InvocationTargetException e) {
            Throwable c = e.getCause();
            StackTraceElement[] st = c.getStackTrace();
            out.append("conf ite ").append(c.getClass().getName()).append(' ')
                    .append(c.getMessage()).append(" wrapperMsg=").append(e.getMessage())
                    .append(" top=").append(st.length > 0 ? st[0].getMethodName() : "<none>")
                    .append(" traced=").append(st.length > 0).append('\n');
        } catch (IllegalAccessException e) {
            out.append("conf ite IAE\n");
        }
        // throw: an explicitly constructed exception keeps its stack trace.
        try {
            thrower(0);
        } catch (IllegalStateException e) {
            StackTraceElement[] st = e.getStackTrace();
            out.append("conf throw ").append(e.getMessage()).append(" top=")
                    .append(st.length > 0 ? st[0].getMethodName() : "<none>").append('\n');
        }
        // rlock
        ReentrantLock l = new ReentrantLock();
        l.lock();
        l.lock();
        out.append("conf rlock held=").append(l.isHeldByCurrentThread()).append(" holds=")
                .append(l.getHoldCount()).append(" locked=").append(l.isLocked());
        l.unlock();
        l.unlock();
        out.append(" after=").append(l.isLocked()).append(" try=").append(l.tryLock());
        l.unlock();
        try {
            l.unlock();
            out.append(" unlockUnheld=none");
        } catch (IllegalMonitorStateException e) {
            out.append(" unlockUnheld=").append(e.getClass().getSimpleName());
        }
        out.append('\n');
        // optional
        out.append("conf optional ").append(Optional.of(3).map(x -> x + 1)).append(' ')
                .append(Optional.empty()).append(' ').append(optionalOp(1)).append(' ')
                .append(optionalOp(0)).append(' ').append(optionalOp(2)).append('\n');
        // weakref
        Object o = new Object();
        ReferenceQueue<Object> q = new ReferenceQueue<>();
        WeakReference<Object> w = new WeakReference<>(o, q);
        PhantomReference<Object> p = new PhantomReference<>(o, q);
        out.append("conf weakref get=").append(w.get() == o).append(" refersTo=")
                .append(w.refersTo(o)).append(" refersToNull=").append(w.refersTo(null))
                .append(" phantomGet=").append(p.get()).append(" phantomRefersTo=")
                .append(p.refersTo(o));
        w.clear();
        out.append(" cleared=").append(w.get()).append(" clearedRefersToNull=")
                .append(w.refersTo(null)).append(" enqueue=").append(w.enqueue())
                .append(" polled=").append(q.poll() == w).append('\n');
        Reference.reachabilityFence(o);
        // date (no toString: it depends on the default time zone)
        Date d = new Date(1_234_567_890_123L);
        out.append("conf date ").append(d.getTime()).append(' ').append(d.toInstant())
                .append(' ').append(d.hashCode()).append(' ')
                .append(d.compareTo(new Date(0L))).append(' ')
                .append(d.equals(new Date(1_234_567_890_123L))).append('\n');
        // the masked tables
        out.append("conf masks fence=").append(fenceRow(5)).append(" hex=").append(hexOp(7))
                .append(" crc=").append(crcOp(9)).append('\n');
    }

    // ---- the timing ----

    interface Op {
        int run(int i);
    }

    static final StringBuilder CHECK = new StringBuilder("check");
    static final StringBuilder OUT = new StringBuilder();

    static int loop(Op op, int n) {
        int h = 0;
        for (int i = 0; i < n; i++) {
            h += op.run(i);
        }
        return h;
    }

    // Two untimed runs, then one timed run (as Nio20FenceRow's `row`).
    static void row(String name, int n, Op op) {
        if (!on(name)) {
            return;
        }
        int h = loop(op, n);
        h ^= loop(op, n);
        long t0 = System.nanoTime();
        h ^= loop(op, n);
        long ns = (System.nanoTime() - t0) / n;
        CHECK.append(' ').append(name).append('=').append(h);
        OUT.append(name).append(' ').append(ns).append(" ns/op\n");
    }

    public static void main(String[] args) {
        StringBuilder conf = new StringBuilder();
        conf(conf);
        System.out.print(conf);
        int slow = Math.max(1, N / 20);
        row("ite", slow, Nio20JitRows::iteOp);
        row("throwlocal", slow, Nio20JitRows::throwLocalOp);
        row("throwcall", slow, Nio20JitRows::throwCallOp);
        row("rlock", N, Nio20JitRows::rlockOp);
        row("optional", N, Nio20JitRows::optionalOp);
        row("weakref", N, Nio20JitRows::weakrefOp);
        row("refget", N, Nio20JitRows::refGetOp);
        row("date", N, Nio20JitRows::dateOp);
        row("fenceown", N, Nio20JitRows::fenceRow);
        row("hexmask", N, Nio20JitRows::hexOp);
        row("crcmask", N, Nio20JitRows::crcOp);
        System.out.println(CHECK);
        System.out.print(OUT);
    }
}
