import java.lang.foreign.*;
import java.util.concurrent.atomic.AtomicLong;

/**
 * SBR4 lane segacc: a shared arena closed while readers and writers are inside compiled `MemorySegment.get/set` loops.
 * Each reader must see either the pattern it was promised (every int of the block equals 0x5A5A5A5A) or the JDK's
 * `IllegalStateException: Already closed` -- never a garbage value, never a crash. Run many rounds; prints counts that are
 * only stable in their invariants (bad == 0, a closed exception was seen at least once per round on HotSpot and here).
 */
public class Sbr4SegaccRace {
    static int geti(MemorySegment s, long off) { return s.get(ValueLayout.JAVA_INT, off); }
    static void seti(MemorySegment s, long off, int v) { s.set(ValueLayout.JAVA_INT, off, v); }
    static long getl(MemorySegment s, long off) { return s.get(ValueLayout.JAVA_LONG, off); }

    public static void main(String[] args) throws Throwable {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 40;
        AtomicLong bad = new AtomicLong(), closedSeen = new AtomicLong(), okReads = new AtomicLong(), otherEx = new AtomicLong();
        for (int r = 0; r < rounds; r++) {
            Arena a = Arena.ofShared();
            MemorySegment s = a.allocate(4096, 8);
            for (int i = 0; i < 1024; i++) seti(s, (long) i << 2, 0x5A5A5A5A);
            // warm the loops so the reader is compiled before the close
            long w = 0;
            for (int i = 0; i < 30000; i++) w += geti(s, (long) (i & 1023) << 2);
            Thread[] ts = new Thread[3];
            for (int t = 0; t < ts.length; t++) {
                final boolean writer = t == 2;
                ts[t] = new Thread(() -> {
                    try {
                        for (int i = 0; i < 20_000_000; i++) {
                            if (writer) seti(s, (long) (i & 1023) << 2, 0x5A5A5A5A);
                            else {
                                int v = geti(s, (long) (i & 1023) << 2);
                                if (v != 0x5A5A5A5A) { bad.incrementAndGet(); return; }
                                if (getl(s, (long) (i & 511) << 3) != 0x5A5A5A5A5A5A5A5AL) { bad.incrementAndGet(); return; }
                                okReads.incrementAndGet();
                            }
                        }
                    } catch (IllegalStateException e) {
                        if (String.valueOf(e.getMessage()).contains("closed")) closedSeen.incrementAndGet(); else otherEx.incrementAndGet();
                    } catch (Throwable e) { otherEx.incrementAndGet(); }
                });
                ts[t].start();
            }
            Thread.sleep(5 + (r % 7) * 3);
            a.close();
            for (Thread t : ts) t.join();
        }
        System.out.println("bad |" + bad.get() + "|");
        System.out.println("otherEx |" + otherEx.get() + "|");
        System.out.println("closedSeenEveryRound |" + (closedSeen.get() >= rounds) + "|");
        System.out.println("someReads |" + (okReads.get() > 0) + "|");
    }
}
