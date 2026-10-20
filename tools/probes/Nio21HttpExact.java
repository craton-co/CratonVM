// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.nio.ByteBuffer;
import java.nio.channels.Pipe;
import java.nio.channels.SelectionKey;
import java.nio.channels.Selector;
import java.time.Duration;
import java.time.Instant;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;

/**
 * nio21 wave 9, lane L2: the two `HttpClient` request-path costs this lane changed
 * (docs/internal/fixed-bugs/r20-jdk-http-client-throughput-is-ten-to-fifty-x-hotspot-CLOSED-20261012.md,
 * "nio21 wave 9, lane L2").
 *
 * <ul>
 *   <li>{@code CRATONVM_JIT_SPLICE_INTRINSIC_JDK_BODY_EXACT} (default ON): a compiled caller
 *       splices the JDK body of {@code Math.addExact} / {@code subtractExact} /
 *       {@code multiplyExact(II)} / {@code incrementExact} / {@code decrementExact} /
 *       {@code negateExact} / {@code toIntExact} instead of calling the registered
 *       {@code Intrinsic} native; the overflow arm is a deopt guard into the callee's frame, where
 *       the interpreter throws. Rows {@code exact}, {@code exthrow}, {@code exflip},
 *       {@code deadline}; bench rows {@code exadd}, {@code deadline}.</li>
 *   <li>{@code CRATONVM_NIO_EPOLL_EVENTS_REUSE} (default ON, Linux): {@code EPoll.wait} reuses a
 *       per-thread event array instead of allocating and zeroing 12 KiB per wait. Row
 *       {@code select}; bench rows {@code selnow}, {@code wakeup}.</li>
 * </ul>
 *
 * <pre>
 *   javac -d /tmp/p21 tools/probes/Nio21HttpExact.java
 *   # HotSpot's C2 throws a PREALLOCATED, trace-less exception from a hot Math.*Exact site
 *   # (OmitStackTraceInFastThrow): its default output varies run to run ("null @ <none>").
 *   # The reference is therefore taken with the omission off, which equals -Xint.
 *   java -XX:-OmitStackTraceInFastThrow -cp /tmp/p21 Nio21HttpExact > hs.txt
 *   cratonvm --jdk-only   -cp /tmp/p21 Nio21HttpExact > jo.txt
 *   cratonvm --compatible -cp /tmp/p21 Nio21HttpExact > co.txt
 *   CRATONVM_JIT_SPLICE_INTRINSIC_JDK_BODY_EXACT=0 CRATONVM_NIO_EPOLL_EVENTS_REUSE=0 \
 *       cratonvm --jdk-only -cp /tmp/p21 Nio21HttpExact > jo0.txt      # hs = jo = co = jo0
 *   java     -cp /tmp/p21 -Dnio21.bench=1 Nio21HttpExact               # interleave, 5 runs each
 *   cratonvm -cp /tmp/p21 -Dnio21.bench=1 Nio21HttpExact
 *   CRATONVM_JIT_SPLICE_INTRINSIC_JDK_BODY_EXACT=0 cratonvm -cp /tmp/p21 -Dnio21.bench=1 -Dnio21.rows=exadd,deadline Nio21HttpExact
 *   CRATONVM_NIO_EPOLL_EVENTS_REUSE=0 cratonvm -cp /tmp/p21 -Dnio21.bench=1 -Dnio21.rows=selnow,wakeup Nio21HttpExact
 * </pre>
 *
 * {@code -Dnio21.rows=a,b} selects rows (default all). Conformance prints answers only; the bench
 * ({@code -Dnio21.bench=1} or {@code true}) prints one {@code <row> <N> ns/op} line per row, the
 * median of five timed passes after three warm-up passes ({@code -Dnio21.iters=N}, default 200000).
 */
public class Nio21HttpExact {
    static final String ROWS = System.getProperty("nio21.rows", "all");
    static final int WARM = 30_000;

    static boolean on(String r) {
        return ROWS.equals("all") || Arrays.asList(ROWS.split(",")).contains(r);
    }

    static String ex(Throwable t) {
        StackTraceElement[] st = t.getStackTrace();
        String top = st.length == 0 ? "<none>" : st[0].getClassName() + "." + st[0].getMethodName();
        return t.getClass().getName() + ": " + t.getMessage() + " @ " + top;
    }

    // ---- the ops (each a compiled caller of the Math rows) ----------------------------------

    static long exactOp(int i, long l) {
        long s = Math.addExact(i, 7) + Math.subtractExact(i, 3) + Math.multiplyExact(i & 1023, 31);
        s += Math.incrementExact(i) + Math.decrementExact(i) + Math.negateExact(i);
        s += Math.addExact(l, 11L) + Math.subtractExact(l, 5L) + Math.incrementExact(l)
                + Math.decrementExact(l) + Math.negateExact(l) + Math.toIntExact(l & 0xffff);
        return s;
    }

    static int addI(int a, int b) {
        return Math.addExact(a, b);
    }

    static long addL(long a, long b) {
        return Math.addExact(a, b);
    }

    static int mulI(int a, int b) {
        return Math.multiplyExact(a, b);
    }

    static int incI(int a) {
        return Math.incrementExact(a);
    }

    static long decL(long a) {
        return Math.decrementExact(a);
    }

    static int negI(int a) {
        return Math.negateExact(a);
    }

    static int toInt(long a) {
        return Math.toIntExact(a);
    }

    static long subL(long a, long b) {
        return Math.subtractExact(a, b);
    }

    static long deadlineOp(int i) {
        Instant base = Instant.ofEpochSecond(1_790_000_000L, 123_456_789L);
        Duration d = Duration.ofMillis(250 + (i & 1023));
        Instant dl = base.plus(d);
        long until = base.until(dl, java.time.temporal.ChronoUnit.MILLIS);
        return dl.toEpochMilli() + until + d.toMillis() + d.toNanos() + (dl.isAfter(base) ? 1 : 0);
    }

    // ---- conformance -----------------------------------------------------------------------

    static void exact() {
        long sum = 0;
        for (int i = 0; i < WARM; i++) sum += exactOp(i, (long) i * 1_000_003L);
        System.out.println("exact sum " + sum);
        System.out.println("exact edges " + addI(Integer.MAX_VALUE - 1, 1) + " " + addL(Long.MIN_VALUE + 1, -1)
                + " " + mulI(46_340, 46_340) + " " + incI(-1) + " " + decL(Long.MIN_VALUE + 1)
                + " " + negI(Integer.MIN_VALUE + 1) + " " + toInt(Integer.MIN_VALUE) + " "
                + subL(Long.MAX_VALUE, Long.MAX_VALUE));
    }

    interface Thrower {
        long run();
    }

    static void tryRow(String name, Thrower t) {
        try {
            System.out.println("exthrow " + name + " = " + t.run());
        } catch (ArithmeticException e) {
            System.out.println("exthrow " + name + " " + ex(e));
        }
    }

    static void exthrow() {
        tryRow("addI", () -> addI(Integer.MAX_VALUE, 1));
        tryRow("addL", () -> addL(Long.MIN_VALUE, -1));
        tryRow("mulI", () -> mulI(46_341, 46_341));
        tryRow("incI", () -> incI(Integer.MAX_VALUE));
        tryRow("decL", () -> decL(Long.MIN_VALUE));
        tryRow("negI", () -> negI(Integer.MIN_VALUE));
        tryRow("toInt", () -> toInt(1L << 31));
        tryRow("subL", () -> subL(Long.MIN_VALUE, 1));
    }

    /** Warm each caller on the non-overflow path only, then overflow the SAME compiled site. */
    static void exflip() {
        long sum = 0;
        for (int round = 0; round < 3; round++) {
            for (int i = 0; i < WARM; i++) {
                sum += addI(i, round) + addL(i, -round) + mulI(i & 255, 3) + incI(i) + decL(i)
                        + negI(i) + toInt(i) + subL(i, round);
            }
            int thrown = 0;
            String last = "";
            for (int k = 0; k < 16; k++) {
                try {
                    sum += addI(Integer.MAX_VALUE - k % 2, 1 + k % 2);
                } catch (ArithmeticException e) {
                    thrown++;
                    last = ex(e);
                }
                try {
                    sum += toInt(Long.MAX_VALUE - k);
                } catch (ArithmeticException e) {
                    thrown++;
                    last = last + " | " + ex(e);
                }
            }
            System.out.println("exflip round " + round + " thrown=" + thrown + " " + last);
        }
        System.out.println("exflip sum " + sum);
    }

    static void deadline() {
        long sum = 0;
        for (int i = 0; i < WARM; i++) sum += deadlineOp(i);
        System.out.println("deadline sum " + sum);
        try {
            System.out.println("deadline max " + Instant.MAX.plus(Duration.ofSeconds(1)));
        } catch (java.time.DateTimeException | ArithmeticException e) {
            System.out.println("deadline max " + e.getClass().getName() + ": " + e.getMessage());
        }
        try {
            System.out.println("deadline millis " + Duration.ofSeconds(Long.MAX_VALUE / 100).toMillis());
        } catch (ArithmeticException e) {
            System.out.println("deadline millis " + ex(e));
        }
    }

    // ---- the selector ----------------------------------------------------------------------

    static Pipe pipe;
    static Selector sel;
    static final ByteBuffer ONE = ByteBuffer.allocate(1);
    static final ByteBuffer DRAIN = ByteBuffer.allocate(64);

    static void openPipe() throws Exception {
        if (pipe != null) return;
        pipe = Pipe.open();
        pipe.source().configureBlocking(false);
        sel = Selector.open();
        pipe.source().register(sel, SelectionKey.OP_READ);
    }

    /** One byte through the pipe, one {@code selectNow}, one drain: the selected-key count. */
    static int selNowOp() throws Exception {
        ONE.clear();
        ONE.put((byte) 1).flip();
        pipe.sink().write(ONE);
        int n = sel.selectNow();
        sel.selectedKeys().clear();
        DRAIN.clear();
        int r = pipe.source().read(DRAIN);
        int idle = sel.selectNow();
        return n * 10 + r + idle;
    }

    /** {@code wakeup()} then a blocking {@code select(1000)}: the EventFD set / wait / drain. */
    static int wakeupOp() throws Exception {
        sel.wakeup();
        return sel.select(1000);
    }

    static void select() throws Exception {
        openPipe();
        long sum = 0;
        for (int i = 0; i < 2_000; i++) sum += selNowOp() + wakeupOp();
        System.out.println("select sum " + sum + " keys " + sel.keys().size());
    }

    // ---- bench -----------------------------------------------------------------------------

    interface Op {
        long run(int i) throws Exception;
    }

    static final List<String> TIMES = new ArrayList<>();

    static void bench(String name, int n, Op op) throws Exception {
        if (!on(name)) return;
        long sink = 0;
        for (int w = 0; w < 3; w++) for (int i = 0; i < n; i++) sink += op.run(i);
        long[] t = new long[5];
        for (int r = 0; r < t.length; r++) {
            long t0 = System.nanoTime();
            for (int i = 0; i < n; i++) sink += op.run(i);
            t[r] = System.nanoTime() - t0;
        }
        Arrays.sort(t);
        TIMES.add(name + " " + (t[2] / n) + " ns/op" + (sink == 42 ? " " : ""));
    }

    public static void main(String[] args) throws Exception {
        String b = System.getProperty("nio21.bench", "");
        if (b.equals("1") || b.equalsIgnoreCase("true")) {
            int n = Integer.getInteger("nio21.iters", 200_000);
            bench("exadd", n, i -> exactOp(i, (long) i * 1_000_003L));
            bench("deadline", n, Nio21HttpExact::deadlineOp);
            if (on("selnow") || on("wakeup")) openPipe();
            bench("selnow", Math.max(1, n / 20), i -> selNowOp());
            bench("wakeup", Math.max(1, n / 20), i -> wakeupOp());
            for (String t : TIMES) System.out.println(t);
            return;
        }
        if (on("exflip")) exflip(); // first: nothing has overflowed a compiled site before it
        if (on("exact")) exact();
        if (on("exthrow")) exthrow();
        if (on("deadline")) deadline();
        if (on("select")) select();
    }
}
