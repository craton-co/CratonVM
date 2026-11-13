// j27 wave 3 (lane SMALLCALL): small JDK methods behind the Tomcat
// TestMethodPerformance / CharsetDecoder.decode gap.
// Page: docs/known-issues/jit/small-jdk-method-call-overhead-charsetdecoder-decode-25-80x-tomcat-methodperformance-and-http2-8-2-wall-20261025.md
//
// Every `fact` row is a computed value or an exception class + message; none
// depends on timing or on which tier ran. Buffer setters are called through
// the abstract `Buffer` type (the covariant bridge `ByteBuffer.position(I)Buffer`
// -> `position(I)ByteBuffer` -> `Buffer.position(I)Buffer`), through the
// covariant `ByteBuffer` / `CharBuffer` type, and from the `Buffer.<init>`
// chain of `ByteBuffer.wrap`. The `hot-*` rows re-ask an edge case after the
// helpers ran WARM times, so a compiled (spliced) setter must still build the
// JDK's own exception message.
//
//   java -cp <dir> J27SmallCallRows                  > hs.txt
//   cratonvm --cp <dir> J27SmallCallRows | diff - hs.txt
//   (and with --compatible, and once with --nojit)
// Timing (separate `time` lines, ns/op, median of 5 reps of 2,000,000):
//   java -Dj27.timing=true -cp <dir> J27SmallCallRows
//
// Expected HotSpot 25 output (facts only):
//   fact wrap-0-3 pos=0 lim=3 cap=5 rem=3 off=0
//   fact wrap-oob java.lang.IndexOutOfBoundsException null
//   fact pos-over java.lang.IllegalArgumentException newPosition > limit: (9 > 5)
//   fact pos-neg java.lang.IllegalArgumentException newPosition < 0: (-1 < 0)
//   fact lim-over java.lang.IllegalArgumentException newLimit > capacity: (6 > 5)
//   fact lim-neg java.lang.IllegalArgumentException newLimit < 0: (-2 < 0)
//   fact abstract-pos-over java.lang.IllegalArgumentException newPosition > limit: (9 > 5)
//   fact covariant-class java.nio.HeapByteBuffer same=true
//   fact covariant-char-class java.nio.HeapCharBuffer pos=3
//   fact limit-shrink pos=2 lim=2
//   fact limit-shrink-reset java.nio.InvalidMarkException null
//   fact iso-decode 97,98,233 in=3/3 out=0/3/3
//   fact utf8-report java.nio.charset.MalformedInputException Input length = 1 inpos=1
//   fact utf8-truncated java.nio.charset.MalformedInputException Input length = 1 inpos=1
//   fact utf8-string-replace 97,65533,40
//   fact utf8-decoder-replace 97,65533,40
//   fact warm-wrap 2199998
//   fact warm-setters 2300000
//   fact warm-chain 500000
//   fact warm-charbuf 1200000
//   fact warm-iso-decode 47200000
//   fact warm-sb 1700000
//   fact hot-pos-over java.lang.IllegalArgumentException newPosition > limit: (99 > 16)
//   fact hot-lim-neg java.lang.IllegalArgumentException newLimit < 0: (-3 < 0)
//   fact hot-wrap-oob java.lang.IndexOutOfBoundsException null
//   fact hot-iso-decode 97,98,233 in=3/3 out=0/3/3
//   fact hot-utf8-report java.nio.charset.MalformedInputException Input length = 1 inpos=1
import java.nio.Buffer;
import java.nio.ByteBuffer;
import java.nio.CharBuffer;
import java.nio.charset.CharacterCodingException;
import java.nio.charset.CharsetDecoder;
import java.nio.charset.CodingErrorAction;
import java.nio.charset.StandardCharsets;

public class J27SmallCallRows {
    static final int WARM = 200_000;
    static final byte[] IN = {0x61, 0x62, (byte) 0xE9, 0x64, 0x65};
    static final byte[] BAD = {0x61, (byte) 0xC3, 0x28};
    static final byte[] TRUNC = {0x61, (byte) 0xC3};
    static ByteBuffer held = ByteBuffer.allocate(16);
    static CharBuffer heldChars = CharBuffer.allocate(8);
    static Object sink;

    static void fact(String name, Object value) {
        System.out.println("fact " + name + " " + value);
    }

    static String ex(Throwable t) {
        return t.getClass().getName() + " " + t.getMessage();
    }

    static String codePoints(CharSequence s) {
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < s.length(); i++) {
            if (i > 0) sb.append(',');
            sb.append((int) s.charAt(i));
        }
        return sb.toString();
    }

    // ---- helpers the warm loops make hot ----

    static ByteBuffer wrap(int off, int len) {
        return ByteBuffer.wrap(IN, off, len);
    }

    static int setPos(Buffer b, int p) {
        return b.position(p).position();
    }

    static int setLim(Buffer b, int l) {
        return b.limit(l).limit();
    }

    static String isoDecode(CharsetDecoder dec) throws CharacterCodingException {
        ByteBuffer in = wrap(0, 3);
        CharBuffer out = dec.decode(in);
        return codePoints(out) + " in=" + in.position() + "/" + in.limit()
                + " out=" + out.position() + "/" + out.limit() + "/" + out.capacity();
    }

    static String utf8Report(byte[] bytes) {
        CharsetDecoder dec = StandardCharsets.UTF_8.newDecoder()
                .onMalformedInput(CodingErrorAction.REPORT)
                .onUnmappableCharacter(CodingErrorAction.REPORT);
        ByteBuffer in = ByteBuffer.wrap(bytes);
        try {
            sink = dec.decode(in);
            return "no-exception";
        } catch (CharacterCodingException e) {
            return ex(e) + " inpos=" + in.position();
        }
    }

    static String tryWrap(int off, int len) {
        try {
            sink = wrap(off, len);
            return "no-exception";
        } catch (RuntimeException e) {
            return ex(e);
        }
    }

    // ---- warm loops ----

    static long warmWrap(int n) {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            ByteBuffer b = wrap(i % 3, 2);
            sum += b.position() + b.limit() + b.remaining() + b.capacity();
        }
        return sum;
    }

    static long warmSetters(int n) {
        Buffer b = held;
        long sum = 0;
        for (int i = 0; i < n; i++) {
            setLim(b, 16);
            setPos(b, i & 7);
            setLim(b, 8 + (i & 7));
            sum += b.remaining() + b.position();
        }
        return sum;
    }

    static long warmChain(int n) {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            ByteBuffer r = held.limit(16).position(i & 3);
            sum += r.position() + (r == held ? 1 : 0);
        }
        return sum;
    }

    static long warmCharBuf(int n) {
        CharBuffer cb = heldChars;
        long sum = 0;
        for (int i = 0; i < n; i++) {
            cb.limit(8);
            cb.position(i % 5);
            sum += cb.remaining();
        }
        return sum;
    }

    static long warmIsoDecode(CharsetDecoder dec, int n) throws CharacterCodingException {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            dec.reset();
            CharBuffer out = dec.decode(ByteBuffer.wrap(IN, 0, 3));
            sum += out.charAt(2) + out.remaining();
        }
        return sum;
    }

    static long warmSb(int n) {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            String s = new StringBuilder("abc").append(i % 10).toString();
            sum += s.length() + (s.charAt(3) - '0');
        }
        return sum;
    }

    static void coldFacts() throws CharacterCodingException {
        ByteBuffer w = wrap(0, 3);
        fact("wrap-0-3", "pos=" + w.position() + " lim=" + w.limit() + " cap=" + w.capacity()
                + " rem=" + w.remaining() + " off=" + w.arrayOffset());
        fact("wrap-oob", tryWrap(4, 3));

        ByteBuffer bb = ByteBuffer.allocate(5);
        try { bb.position(9); fact("pos-over", "no-exception"); }
        catch (IllegalArgumentException e) { fact("pos-over", ex(e)); }
        try { bb.position(-1); fact("pos-neg", "no-exception"); }
        catch (IllegalArgumentException e) { fact("pos-neg", ex(e)); }
        try { bb.limit(6); fact("lim-over", "no-exception"); }
        catch (IllegalArgumentException e) { fact("lim-over", ex(e)); }
        try { bb.limit(-2); fact("lim-neg", "no-exception"); }
        catch (IllegalArgumentException e) { fact("lim-neg", ex(e)); }
        Buffer abs = bb;
        try { abs.position(9); fact("abstract-pos-over", "no-exception"); }
        catch (IllegalArgumentException e) { fact("abstract-pos-over", ex(e)); }

        Buffer r = abs.position(1);
        fact("covariant-class", r.getClass().getName() + " same=" + (r == bb));
        CharBuffer cr = CharBuffer.allocate(8).position(3);
        fact("covariant-char-class", cr.getClass().getName() + " pos=" + cr.position());

        bb.clear();
        bb.position(4);
        bb.mark();
        bb.limit(2);
        fact("limit-shrink", "pos=" + bb.position() + " lim=" + bb.limit());
        try { bb.reset(); fact("limit-shrink-reset", "no-exception"); }
        catch (RuntimeException e) { fact("limit-shrink-reset", ex(e)); }

        fact("iso-decode", isoDecode(StandardCharsets.ISO_8859_1.newDecoder()));
        fact("utf8-report", utf8Report(BAD));
        fact("utf8-truncated", utf8Report(TRUNC));
        fact("utf8-string-replace", codePoints(new String(BAD, StandardCharsets.UTF_8)));
        CharsetDecoder rep = StandardCharsets.UTF_8.newDecoder()
                .onMalformedInput(CodingErrorAction.REPLACE);
        fact("utf8-decoder-replace", codePoints(rep.decode(ByteBuffer.wrap(BAD))));
    }

    static void hotFacts() throws CharacterCodingException {
        ByteBuffer b = ByteBuffer.allocate(16);
        try { setPos(b, 99); fact("hot-pos-over", "no-exception"); }
        catch (IllegalArgumentException e) { fact("hot-pos-over", ex(e)); }
        try { setLim(b, -3); fact("hot-lim-neg", "no-exception"); }
        catch (IllegalArgumentException e) { fact("hot-lim-neg", ex(e)); }
        fact("hot-wrap-oob", tryWrap(4, 3));
        CharsetDecoder dec = StandardCharsets.ISO_8859_1.newDecoder();
        String iso = "";
        for (int i = 0; i < WARM / 10; i++) iso = isoDecode(dec);
        fact("hot-iso-decode", iso);
        String rep = "";
        for (int i = 0; i < WARM / 100; i++) rep = utf8Report(BAD);
        fact("hot-utf8-report", rep);
    }

    interface Row { long run(int n) throws Exception; }

    static void time(String name, Row row) throws Exception {
        final int n = 2_000_000;
        long[] ns = new long[5];
        for (int rep = 0; rep < ns.length; rep++) {
            long t0 = System.nanoTime();
            sink = row.run(n);
            ns[rep] = System.nanoTime() - t0;
        }
        java.util.Arrays.sort(ns);
        System.out.printf("time %s %.1f ns/op%n", name, ns[2] / (double) n);
    }

    public static void main(String[] args) throws Exception {
        coldFacts();
        CharsetDecoder dec = StandardCharsets.ISO_8859_1.newDecoder();
        fact("warm-wrap", warmWrap(WARM));
        fact("warm-setters", warmSetters(WARM));
        fact("warm-chain", warmChain(WARM));
        fact("warm-charbuf", warmCharBuf(WARM));
        fact("warm-iso-decode", warmIsoDecode(dec, WARM));
        fact("warm-sb", warmSb(WARM));
        hotFacts();
        if (Boolean.getBoolean("j27.timing")) {
            time("wrap", J27SmallCallRows::warmWrap);
            time("setters", J27SmallCallRows::warmSetters);
            time("chain", J27SmallCallRows::warmChain);
            time("charbuf", J27SmallCallRows::warmCharBuf);
            time("iso-decode", n -> warmIsoDecode(dec, n));
            time("sb", J27SmallCallRows::warmSb);
        }
    }
}
