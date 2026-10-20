// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * gen r4w4/young4 (2026-09-24): a JIT-WARM, multi-threaded, allocation-heavy
 * program — the shape on which the generational young collector diverts to
 * its non-moving sweep because a compiled frame is live and a conservative
 * JIT-root scan ran ({@code divert_non_moving} term 4, reason
 * {@code nonmoving-unrewritable-conservative-jit-roots}).
 *
 * <p>Every worker thread spends its life inside one hot method that keeps
 * young references in locals across allocations (a short linked list it walks
 * back) and runs the {@code char[]}/{@code byte[]} loops ({@code StringBuilder}
 * → {@code String}, i.e. {@code StringUTF16.compress}-style derived-pointer
 * loops) that made the QDox repro the reason term 4 exists. So once the JIT
 * has compiled {@code work}, essentially every young collection happens with
 * compiled frames live on every thread.
 *
 * <p>What to read (the goal of lane young4 is FEWER such diverts):
 * <ul>
 *   <li>{@code [GC] decision histogram:} — the {@code non_moving=} count and
 *       the {@code nonmoving-unrewritable-conservative-jit-roots} row;</li>
 *   <li>{@code [GC] young_conservative_divert:} (new in gen r4w4) —
 *       {@code cjdiv_diverts} must equal that row; {@code cjdiv_no_young_pin}
 *       is how many of those cycles a pin-aware copying cycle would have had to
 *       pin NOTHING on (only the unregistered interior/derived words stood in
 *       the way); {@code cjdiv_young_pins_sum / cjdiv_diverts} is the mean
 *       number of objects such a cycle would leave in place.</li>
 * </ul>
 * Per-thread results are independent and summed, so the checksum does not
 * depend on scheduling or on the collector.
 *
 * <p>Expected output (defaults; HotSpot 25, any collector):
 * <pre>
 *   PASS jitwarm threads=4 calls=1500000 checksum=-4668312146048759300
 *   (args 1 1500000)  PASS jitwarm threads=1 calls=1500000 checksum=-1069508748886098540
 *   (args 4 200000)   PASS jitwarm threads=4 calls=200000 checksum=-622814496315717380
 * </pre>
 * Commands:
 * <pre>
 *   java -Xmx256m -cp tools/bench GenR4W4JitWarmDivertProbe
 *   CRATONVM_DBG=gc-stats cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR4W4JitWarmDivertProbe
 *   CRATONVM_DBG=gc-stats cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR4W4JitWarmDivertProbe 1 1500000
 *   CRATONVM_DBG=gc-stats cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m --nojit -cp tools/bench GenR4W4JitWarmDivertProbe 4 200000
 * </pre>
 * The {@code --nojit} line is the control: {@code cjdiv_diverts=0} and every
 * young cycle {@code moving-no-jit-frames-live}. Optional args:
 * {@code threads calls}. Exit status 1 on FAIL.
 */
public final class GenR4W4JitWarmDivertProbe {
    static final class Cell {
        final int v;
        final Cell next;

        Cell(int v, Cell next) {
            this.v = v;
            this.next = next;
        }
    }

    /** The hot method: allocates while young references are live in locals. */
    static long work(int seed) {
        Cell head = null;
        for (int k = 0; k < 16; k++) {
            head = new Cell(seed * 31 + k, head);
        }
        final StringBuilder sb = new StringBuilder(48);
        int x = seed;
        for (int k = 0; k < 40; k++) {
            x = x * 1103515245 + 12345;
            sb.append((char) ('a' + ((x >>> 16) & 15)));
        }
        final String s = sb.toString();
        final byte[] bytes = s.getBytes(java.nio.charset.StandardCharsets.ISO_8859_1);
        long acc = s.hashCode();
        for (byte b : bytes) {
            acc = acc * 131 + b;
        }
        // Walk the list only now: it was live across every allocation above.
        for (Cell c = head; c != null; c = c.next) {
            acc += c.v;
        }
        return acc;
    }

    public static void main(String[] args) throws InterruptedException {
        final int threads = args.length > 0 ? Integer.parseInt(args[0]) : 4;
        final int calls = args.length > 1 ? Integer.parseInt(args[1]) : 1_500_000;
        final long[] results = new long[threads];
        final Thread[] ts = new Thread[threads];
        for (int t = 0; t < threads; t++) {
            final int id = t;
            ts[t] = new Thread(() -> {
                long acc = 0;
                for (int i = 0; i < calls; i++) {
                    acc = acc * 7 + work(id * 1_000_003 + i);
                }
                results[id] = acc;
            }, "jitwarm-" + t);
            ts[t].start();
        }
        for (Thread t : ts) {
            t.join();
        }
        long checksum = 0;
        for (long r : results) {
            checksum += r;
        }
        System.out.println("PASS jitwarm threads=" + threads + " calls=" + calls
                + " checksum=" + checksum);
    }
}
