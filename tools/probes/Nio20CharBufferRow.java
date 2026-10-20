import java.nio.BufferOverflowException;
import java.nio.CharBuffer;
import java.nio.ReadOnlyBufferException;
import java.util.*;
import java.util.function.Supplier;

/**
 * nio20 wave 6, lane L2: the `heapcb` row's three costs -- `HeapCharBuffer.put(String,int,int)`
 * kept off the optimizing tier by its `String.getChars` site, `String.getChars` ->
 * `StringLatin1.inflate` crossing the native door, and the conservative pin probe
 * (`ZgcRealHeap::resolve_interior_for_pin`) at every young collection.
 *
 * A/B arms: default vs `CRATONVM_JIT_IR_STRING_GETCHARS_PREFIX=0` (the getChars / inflate prefix
 * and its planning-gate and splice carve-outs), and `CRATONVM_JIT_IR_STRING_SSE2=0` for the copy's
 * 8-char step. The pin-probe change has no switch (it changes no answer); read it from `perf`
 * (`resolve_interior_for_pin` self %) or the `xt_pass_us=` of the pause line under
 * `CRATONVM_GC_STATS=1`.
 *
 * Bench rows print `<row> <N> ns/op`; `conf` prints deterministic facts to diff against HotSpot 25
 * (run it in both `--jdk-only` and `--compatible`). `-Dnio20.rows=a,b` selects rows (default all),
 * `-Dnio20.ops=N` the op count (default 5_000_000).
 *
 *   cbput-string  CharBuffer.put(String,int,int) of a 16-char LATIN1 string into a heap buffer
 *   cbsubseq      the R17 heapcb shape: put, flip, subSequence view, its length() + charAt()
 *   getchars      String.getChars(int,int,char[],int), 24 chars, varying offsets
 *   inflate       String.toCharArray() of a 24-char LATIN1 string (StringLatin1.toChars -> inflate)
 *   conf          getChars / put(String..) / toCharArray answers and exceptions over LATIN1, UTF16,
 *                 empty, every bound edge and a null destination, after warm-up (hot sites)
 */
public class Nio20CharBufferRow {
    static String rows = System.getProperty("nio20.rows", "all");
    static long ops = Long.getLong("nio20.ops", 5_000_000L);
    static boolean on(String r) { return rows.equals("all") || Arrays.asList(rows.split(",")).contains(r); }
    static volatile Object sink;

    static final String L16 = "abcdefghijklmnop";
    static final String L24 = "the quick brown fox jump";
    // UTF16: an o-umlaut and a Cyrillic ef, built from code points so the source stays ASCII.
    static final String U24 = "the quick br" + (char) 0xF6 + "wn " + (char) 0x444 + "ox jump";

    // ---- bench bodies (kept out of line so each tiers up on its own) ----

    static long cbPutLoop(long n) {
        CharBuffer cb = CharBuffer.allocate(64);
        long acc = 0;
        for (long i = 0; i < n; i++) {
            cb.clear();
            cb.put(L16, 0, 16);
            acc += cb.position();
        }
        return acc;
    }

    static long cbSubSeqLoop(long n) {
        CharBuffer cb = CharBuffer.allocate(64);
        long acc = 0;
        for (long i = 0; i < n; i++) {
            cb.clear();
            cb.put(L16, 0, 16);
            cb.flip();
            CharSequence sub = cb.subSequence(2, 10);
            acc += sub.length() + sub.charAt((int) (i & 7));
        }
        return acc;
    }

    static long getCharsLoop(long n) {
        char[] dst = new char[64];
        long acc = 0;
        for (long i = 0; i < n; i++) {
            int b = (int) (i & 7);
            L24.getChars(b, b + 16, dst, (int) (i & 15));
            acc += dst[(int) (i & 31)];
        }
        return acc;
    }

    static long inflateLoop(long n) {
        long acc = 0;
        for (long i = 0; i < n; i++) {
            char[] c = L24.toCharArray();
            acc += c[(int) (i & 15)];
        }
        return acc;
    }

    static void bench(String row, Supplier<Long> body) {
        sink = body.get();
        long t0 = System.nanoTime();
        sink = body.get();
        long t1 = System.nanoTime();
        System.out.println(row + " " + ((t1 - t0) / Math.max(1, ops)) + " ns/op");
    }

    // ---- conformance ----

    /** Print with every non-ASCII char as `\\uXXXX`, so the diff never depends on stdout.encoding. */
    static void out(String s) {
        StringBuilder sb = new StringBuilder(s.length());
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c >= 0x20 && c < 0x7F) sb.append(c);
            else sb.append(String.format("\\u%04x", (int) c));
        }
        System.out.println(sb);
    }

    /** One getChars call, answered as text: the copied chars, or the exception and its message. */
    static String gc(String s, int b, int e, char[] dst, int db) {
        try {
            s.getChars(b, e, dst, db);
            return new String(dst);
        } catch (RuntimeException x) {
            return x.getClass().getName() + ": " + x.getMessage();
        }
    }

    /**
     * The same call answered by exception class only. A null destination thrown from a hot,
     * compiled site has no stable message on HotSpot: once C2 has seen enough null traps at
     * `String.getChars`'s `dst.length` it throws the preallocated message-less NPE
     * (`-XX:+OmitStackTraceInFastThrow`, default ON), and when that happens depends on compile
     * timing, so two HotSpot runs disagree. CratonVM's default is `-XX:-OmitStackTraceInFastThrow`
     * (`CRATONVM_OMIT_STACK_TRACE_IN_FAST_THROW` is opt-in). The helpful message is checked cold,
     * before any trap, where every HotSpot configuration agrees.
     */
    static String gcClass(String s, int b, int e, char[] dst, int db) {
        try {
            s.getChars(b, e, dst, db);
            return new String(dst);
        } catch (RuntimeException x) {
            return x.getClass().getName();
        }
    }

    static String put(CharBuffer cb, String s, int b, int e) {
        try {
            cb.clear();
            cb.put(s, b, e);
            return "pos=" + cb.position() + " " + cb.flip();
        } catch (RuntimeException x) {
            return x.getClass().getName() + ": " + x.getMessage();
        }
    }

    static void conf() {
        String[] srcs = {L24, U24, "", "x"};
        int[][] bounds = {
            {0, 24, 0}, {3, 9, 2}, {0, 0, 0}, {5, 5, 31}, {24, 24, 32}, {0, 24, 8}, // last: exactly fits
            {-1, 4, 0}, {4, 3, 0}, {0, 25, 0}, {0, 4, -1}, {0, 24, 9}, {2, 10, 30}, {0, 1, 32},
        };
        CharBuffer cb = CharBuffer.allocate(20);
        CharBuffer ro = CharBuffer.allocate(20).asReadOnlyBuffer();
        // Cold: the helpful null-destination message, before any compiled site has trapped on it.
        for (String s : srcs) out("conf getChars null dst cold [" + s + "] -> " + gc(s, 0, 0, null, 0));
        // Hot: the answers below are taken after every site has run enough to be compiled.
        long check = 0;
        for (int round = 0; round < 3; round++) {
            for (int i = 0; i < 120_000; i++) {
                String s = srcs[i & 3];
                int[] bd = bounds[i % bounds.length];
                char[] dst = new char[32];
                check += gc(s, bd[0], bd[1], dst, bd[2]).hashCode();
                check += put(cb, s, i % 7, 4 + i % 13).hashCode();
                check += s.toCharArray().length;
                if ((i & 1023) == 0) check += gcClass(s, 0, 1, null, 0).hashCode();
            }
            System.gc();
        }
        out("conf check " + check);
        for (String s : srcs) {
            for (int[] bd : bounds) {
                char[] dst = new char[32];
                Arrays.fill(dst, '.');
                out("conf getChars [" + s + "] " + Arrays.toString(bd) + " -> " + gc(s, bd[0], bd[1], dst, bd[2]));
            }
            out("conf getChars null dst hot [" + s + "] -> " + gcClass(s, 0, 0, null, 0));
            out("conf toCharArray [" + s + "] -> " + new String(s.toCharArray()));
            for (int[] pe : new int[][] {{0, 4}, {2, 20}, {0, 21}, {5, 3}, {-1, 2}, {0, 0}}) {
                out("conf put [" + s + "] " + Arrays.toString(pe) + " -> " + put(cb, s, pe[0], pe[1]));
            }
            out("conf put readonly [" + s + "] -> " + put(ro, s, 0, Math.min(1, s.length())));
        }
        try {
            CharBuffer small = CharBuffer.allocate(3);
            small.put(L24, 0, 4);
            out("conf overflow none");
        } catch (BufferOverflowException e) {
            out("conf overflow " + e.getClass().getName());
        }
        try {
            ro.put("ab", 0, 1);
            out("conf ro none");
        } catch (ReadOnlyBufferException e) {
            out("conf ro " + e.getClass().getName());
        }
        // A view into the middle of a larger buffer, written through put(String,..) and read back
        // through a subSequence (the R17 heapcb shape), plus a wrapped char[] with an array offset.
        CharBuffer big = CharBuffer.allocate(40);
        big.position(7);
        CharBuffer slice = big.slice();
        slice.put(L24, 4, 20);
        slice.flip();
        out("conf slice " + slice.subSequence(1, 9) + " big=" + new String(big.array(), 6, 18));
        char[] backing = new char[30];
        CharBuffer wrapped = CharBuffer.wrap(backing, 5, 20).slice();
        wrapped.put(U24, 10, 22);
        out("conf wrapped " + new String(backing).replace('\0', '_'));
    }

    public static void main(String[] a) {
        if (on("conf")) conf();
        if (on("cbput-string")) bench("cbput-string", () -> cbPutLoop(ops));
        if (on("cbsubseq")) bench("cbsubseq", () -> cbSubSeqLoop(ops));
        if (on("getchars")) bench("getchars", () -> getCharsLoop(ops));
        if (on("inflate")) bench("inflate", () -> inflateLoop(ops));
        System.out.println("done");
    }
}
