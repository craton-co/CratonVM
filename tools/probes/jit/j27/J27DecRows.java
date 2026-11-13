// j27 wave 5 (lane DECLOOP): the `dec` row of tools/probes/DecLoop.java taken
// apart. Page: docs/known-issues/jit/small-jdk-method-call-overhead-charsetdecoder-decode-25-80x-tomcat-methodperformance-and-http2-8-2-wall-20261025.md
//
// DecLoop runs all four rows in one OSR'd `main`, and that `main` is pinned to
// the single-pass backend by its StringBuilder row, so every row there pays
// the single-pass caller (calls, a helper `putstatic`). Here each row is its
// own static method, called once per rep, so the row is compiled the way a
// Tomcat caller of `CharsetDecoder.decode` is (a normal method compile).
//
// Rows (ns/op, `time <row> <ns>` lines, only with -Dj27.timing=true):
//   dec     dec.reset(); decode(ByteBuffer.wrap(in,0,3))   (DecLoop's row)
//   decbb   dec.decode(held ByteBuffer reset to 0..3)       (no wrap)
//   dec3    dec.reset(); decode(wrap, held CharBuffer, true); flush  (no CharBuffer.allocate, no flip)
//   wrap    ByteBuffer.wrap(in,0,3)
//   alloc   CharBuffer.allocate(3)
//
//   java -cp <dir> J27DecRows                  > hs.txt
//   cratonvm --cp <dir> J27DecRows | diff - hs.txt   (and --compatible, --nojit)
//   java -Dj27.timing=true -cp <dir> J27DecRows [n] [reps]
//
// Expected HotSpot 25 output (facts only):
//   fact dec GET pos=0 lim=3 cap=3 cls=java.nio.HeapCharBuffer
//   fact decbb GET in=3/3
//   fact dec3 UNDERFLOW UNDERFLOW out=3 GET
//   fact iso-high 233,255,128
//   fact reset-twice OK
//   fact flush-before-decode java.lang.IllegalStateException Current state = RESET, new state = FLUSHED
//   fact warm-dec 1800000
//   fact warm-decbb 1800000
//   fact warm-dec3 1800000
//   fact warm-wrap 1800000
//   fact warm-alloc 1800000
import java.nio.ByteBuffer;
import java.nio.CharBuffer;
import java.nio.charset.CharsetDecoder;
import java.nio.charset.CoderResult;
import java.nio.charset.StandardCharsets;

public class J27DecRows {
    static final byte[] IN = {'G', 'E', 'T'};
    static Object osink;

    static long dec(CharsetDecoder d, int n) throws Exception {
        long s = 0;
        for (int i = 0; i < n; i++) {
            d.reset();
            CharBuffer cb = d.decode(ByteBuffer.wrap(IN, 0, 3));
            osink = cb;
            s += cb.remaining();
        }
        return s;
    }

    static long decbb(CharsetDecoder d, ByteBuffer bb, int n) throws Exception {
        long s = 0;
        for (int i = 0; i < n; i++) {
            bb.clear().limit(3);
            CharBuffer cb = d.decode(bb);
            osink = cb;
            s += cb.remaining();
        }
        return s;
    }

    static long dec3(CharsetDecoder d, CharBuffer out, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            d.reset();
            out.clear();
            ByteBuffer bb = ByteBuffer.wrap(IN, 0, 3);
            CoderResult r = d.decode(bb, out, true);
            if (r.isUnderflow()) {
                r = d.flush(out);
            }
            osink = r;
            s += out.position();
        }
        return s;
    }

    static long wrap(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            ByteBuffer bb = ByteBuffer.wrap(IN, 0, 3);
            osink = bb;
            s += bb.remaining();
        }
        return s;
    }

    static long alloc(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            CharBuffer cb = CharBuffer.allocate(3);
            osink = cb;
            s += cb.capacity();
        }
        return s;
    }

    static void facts(CharsetDecoder d) throws Exception {
        d.reset();
        CharBuffer cb = d.decode(ByteBuffer.wrap(IN, 0, 3));
        System.out.println("fact dec " + cb + " pos=" + cb.position() + " lim=" + cb.limit()
                + " cap=" + cb.capacity() + " cls=" + cb.getClass().getName());
        ByteBuffer bb = ByteBuffer.wrap(IN);
        CharBuffer cb2 = d.decode(bb);
        System.out.println("fact decbb " + cb2 + " in=" + bb.position() + "/" + bb.limit());
        CharBuffer out = CharBuffer.allocate(8);
        d.reset();
        CoderResult r1 = d.decode(ByteBuffer.wrap(IN), out, true);
        CoderResult r2 = d.flush(out);
        out.flip();
        System.out.println("fact dec3 " + r1 + " " + r2 + " out=" + out.remaining() + " " + out);
        CharBuffer hi = d.decode(ByteBuffer.wrap(new byte[] {(byte) 0xE9, (byte) 0xFF, (byte) 0x80}));
        System.out.println("fact iso-high " + (int) hi.get(0) + "," + (int) hi.get(1) + "," + (int) hi.get(2));
        d.reset().reset();
        System.out.println("fact reset-twice OK");
        d.reset();
        try {
            d.flush(CharBuffer.allocate(4));
            System.out.println("fact flush-before-decode no-throw");
        } catch (IllegalStateException e) {
            System.out.println("fact flush-before-decode " + e.getClass().getName() + " " + e.getMessage());
        }
        d.reset();
    }

    public static void main(String[] a) throws Exception {
        int n = a.length > 0 ? Integer.parseInt(a[0]) : 1_000_000;
        int reps = a.length > 1 ? Integer.parseInt(a[1]) : 5;
        boolean timing = Boolean.getBoolean("j27.timing");
        CharsetDecoder d = StandardCharsets.ISO_8859_1.newDecoder();
        ByteBuffer held = ByteBuffer.wrap(IN);
        CharBuffer out = CharBuffer.allocate(8);
        facts(d);
        int warm = 200_000;
        long[] w = new long[5];
        for (int k = 0; k < 3; k++) {
            w[0] += dec(d, warm);
            w[1] += decbb(d, held, warm);
            w[2] += dec3(d, out, warm);
            w[3] += wrap(warm);
            w[4] += alloc(warm);
        }
        System.out.println("fact warm-dec " + w[0]);
        System.out.println("fact warm-decbb " + w[1]);
        System.out.println("fact warm-dec3 " + w[2]);
        System.out.println("fact warm-wrap " + w[3]);
        System.out.println("fact warm-alloc " + w[4]);
        facts2(d);
        if (!timing) {
            return;
        }
        String[] names = {"dec", "decbb", "dec3", "wrap", "alloc"};
        double[][] t = new double[names.length][reps];
        String only = System.getProperty("j27.only", "");
        for (int r = 0; r < reps; r++) {
            for (int row = 0; row < names.length; row++) {
                if (!only.isEmpty() && !only.equals(names[row])) {
                    continue;
                }
                long t0 = System.nanoTime();
                switch (row) {
                    case 0 -> dec(d, n);
                    case 1 -> decbb(d, held, n);
                    case 2 -> dec3(d, out, n);
                    case 3 -> wrap(n);
                    default -> alloc(n);
                }
                t[row][r] = (System.nanoTime() - t0) / (double) n;
            }
        }
        for (int row = 0; row < names.length; row++) {
            double[] v = t[row].clone();
            java.util.Arrays.sort(v);
            System.out.printf(java.util.Locale.ROOT, "time %s %.1f%n", names[row], v[v.length / 2]);
        }
    }

    // After warm-up: the compiled rows must still answer the edge cases the
    // same way (state machine exception from a hot decoder).
    static void facts2(CharsetDecoder d) {
        d.reset();
        try {
            d.flush(CharBuffer.allocate(4));
            System.out.println("fact hot-flush-before-decode no-throw");
        } catch (IllegalStateException e) {
            System.out.println("fact hot-flush-before-decode " + e.getClass().getName() + " " + e.getMessage());
        }
        d.reset();
    }
}
