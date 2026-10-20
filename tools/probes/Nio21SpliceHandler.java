// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// nio21 wave 4, lane L2: the optimizing tier splices a callee whose exception
// table has a REAL handler -- `CharsetDecoder.decode(BB,CB,Z)` and
// `CharsetEncoder.encode(CB,BB,Z)`, whose `decodeLoop` / `encodeLoop` call is
// wrapped in `try { .. } catch (RuntimeException x) { throw new
// CoderMalfunctionError(x); }` -- and covers the `try` with call-site
// exception edges (the callee's handler resumes in the interpreter).
// Switches (all three needed for the splice; the first is the lane's):
//   CRATONVM_JIT_IR_SPLICE_CALLEE_HANDLERS=1
//   CRATONVM_JIT_IR_CALL_SITE_EXCEPTION_EDGES=1
//   CRATONVM_JIT_IR_INLINE_COLD_ATHROW=1      (the two `assert false` arms)
//
//   conf   deterministic text, to diff against HotSpot 25 (both modes): a
//          decoder / encoder subclass whose loop returns normally, throws a
//          RuntimeException (wrapped in CoderMalfunctionError), a
//          java.nio BufferOverflowException (wrapped), an Error (not caught by
//          the callee: propagates as itself) or a NullPointerException
//          (wrapped; its message is not printed), each asked cold, then hot
//          after the caller compiled, and the two must agree. Also the real
//          UTF-8 decoder / encoder on a malformed and a roomy input.
//   rows   `<row> <N> <t> ns/op` lines (one op = one small decode / encode
//          through `decode(BB,CB,Z)` / `encode(CB,BB,Z)`, where the frame
//          the splice removes is a visible share):
//          decode   16 ASCII bytes, UTF-8
//          encode   16 ASCII chars, UTF-8
//
//   java Nio21SpliceHandler conf
//   java -Dnio21.rows=decode,encode Nio21SpliceHandler rows

import java.nio.BufferOverflowException;
import java.nio.ByteBuffer;
import java.nio.CharBuffer;
import java.nio.charset.CharsetDecoder;
import java.nio.charset.CharsetEncoder;
import java.nio.charset.CoderMalfunctionError;
import java.nio.charset.CoderResult;
import java.nio.charset.StandardCharsets;

public class Nio21SpliceHandler {
    static final int HOT = 6_000;
    static Object nullRef;

    /** 0 returns normally; 1 IllegalStateException; 2 BufferOverflowException; 3 AssertionError; 4 NPE. */
    static final class ThrowingDecoder extends CharsetDecoder {
        int mode;

        ThrowingDecoder() {
            super(StandardCharsets.US_ASCII, 1f, 1f);
        }

        @Override
        protected CoderResult decodeLoop(ByteBuffer in, CharBuffer out) {
            switch (mode) {
                case 1:
                    throw new IllegalStateException("boom-ise");
                case 2:
                    throw new BufferOverflowException();
                case 3:
                    throw new AssertionError("boom-error");
                case 4:
                    return CoderResult.malformedForLength(nullRef.hashCode());
                default:
                    while (in.hasRemaining()) {
                        if (!out.hasRemaining()) return CoderResult.OVERFLOW;
                        out.put((char) (in.get() & 0x7f));
                    }
                    return CoderResult.UNDERFLOW;
            }
        }
    }

    static final class ThrowingEncoder extends CharsetEncoder {
        int mode;

        ThrowingEncoder() {
            super(StandardCharsets.US_ASCII, 1f, 1f);
        }

        @Override
        protected CoderResult encodeLoop(CharBuffer in, ByteBuffer out) {
            switch (mode) {
                case 1:
                    throw new IllegalStateException("boom-ise");
                case 2:
                    throw new BufferOverflowException();
                case 3:
                    throw new AssertionError("boom-error");
                case 4:
                    return CoderResult.malformedForLength(nullRef.hashCode());
                default:
                    while (in.hasRemaining()) {
                        if (!out.hasRemaining()) return CoderResult.OVERFLOW;
                        out.put((byte) (in.get() & 0x7f));
                    }
                    return CoderResult.UNDERFLOW;
            }
        }
    }

    static String describe(Throwable t, int mode) {
        if (t instanceof CoderMalfunctionError) {
            Throwable c = t.getCause();
            return "CoderMalfunctionError cause=" + (c == null ? "null" : c.getClass().getName())
                    + (mode == 4 ? "" : " msg=" + t.getMessage());
        }
        return "threw " + t.getClass().getName() + (mode == 4 ? "" : " msg=" + t.getMessage());
    }

    /** The call site the caller compiles: `decode(BB,CB,Z)` with the splice. */
    static String decodeOnce(CharsetDecoder d, byte[] in, int cap) {
        d.reset();
        ByteBuffer bb = ByteBuffer.wrap(in);
        CharBuffer cb = CharBuffer.allocate(cap);
        CoderResult r = d.decode(bb, cb, true);
        return "ok " + r + " src=" + bb.position() + " dst=" + cb.position();
    }

    static String encodeOnce(CharsetEncoder e, String in, int cap) {
        e.reset();
        CharBuffer cb = CharBuffer.wrap(in);
        ByteBuffer bb = ByteBuffer.allocate(cap);
        CoderResult r = e.encode(cb, bb, true);
        return "ok " + r + " src=" + cb.position() + " dst=" + bb.position();
    }

    static String decodeCase(ThrowingDecoder d, int mode) {
        d.mode = mode;
        try {
            return decodeOnce(d, "hello".getBytes(StandardCharsets.US_ASCII), 8);
        } catch (Throwable t) {
            return describe(t, mode);
        } finally {
            d.mode = 0;
        }
    }

    static String encodeCase(ThrowingEncoder e, int mode) {
        e.mode = mode;
        try {
            return encodeOnce(e, "hello", 8);
        } catch (Throwable t) {
            return describe(t, mode);
        } finally {
            e.mode = 0;
        }
    }

    static void conf() {
        ThrowingDecoder d = new ThrowingDecoder();
        ThrowingEncoder e = new ThrowingEncoder();
        String[] coldD = new String[5];
        String[] coldE = new String[5];
        for (int m = 0; m < 5; m++) {
            coldD[m] = decodeCase(d, m);
            coldE[m] = encodeCase(e, m);
        }
        // Compile the callers on the normal path, then ask every mode again
        // (repeatedly, so a deopt and a recompile are both crossed).
        for (int i = 0; i < HOT; i++) {
            decodeCase(d, 0);
            encodeCase(e, 0);
        }
        int lines = 0;
        for (int m = 0; m < 5; m++) {
            String hotD = coldD[m];
            String hotE = coldE[m];
            for (int k = 0; k < 50; k++) {
                hotD = decodeCase(d, m);
                hotE = encodeCase(e, m);
                for (int i = 0; i < 200; i++) {
                    decodeCase(d, 0);
                    encodeCase(e, 0);
                }
            }
            System.out.println("decode mode " + m + ": "
                    + (hotD.equals(coldD[m]) ? hotD : "MISMATCH cold=" + coldD[m] + " hot=" + hotD));
            System.out.println("encode mode " + m + ": "
                    + (hotE.equals(coldE[m]) ? hotE : "MISMATCH cold=" + coldE[m] + " hot=" + hotE));
            lines += 2;
        }
        // The real UTF-8 coders through the same compiled call sites.
        CharsetDecoder utf8d = StandardCharsets.UTF_8.newDecoder();
        CharsetEncoder utf8e = StandardCharsets.UTF_8.newEncoder();
        byte[] malformed = {'a', 'b', (byte) 0xC0, (byte) 0x80, 'c'};
        byte[] text = "héllo wörld".getBytes(StandardCharsets.UTF_8);
        String[] ins = {"plain ascii", "été €", "x\ud800y"};
        String a = "", b = "", c = "";
        for (int i = 0; i < HOT; i++) {
            a = decodeOnce(utf8d, malformed, 8);
            b = decodeOnce(utf8d, text, 32);
            c = encodeOnce(utf8e, ins[i % ins.length], 32);
        }
        System.out.println("utf8 decode malformed: " + a);
        System.out.println("utf8 decode text: " + b);
        System.out.println("utf8 encode last: " + c);
        for (String s : ins) {
            System.out.println("utf8 encode '" + s.length() + "': " + encodeOnce(utf8e, s, 32));
        }
        lines += 3 + ins.length;
        System.out.println("conf lines " + lines);
    }

    static void rows(String spec) {
        for (String row : spec.split(",")) {
            long sink = 0;
            int n = 400_000;
            switch (row) {
                case "decode": {
                    CharsetDecoder dec = StandardCharsets.UTF_8.newDecoder();
                    ByteBuffer bb = ByteBuffer.wrap("0123456789abcdef".getBytes(StandardCharsets.UTF_8));
                    CharBuffer cb = CharBuffer.allocate(16);
                    for (int rep = 0; rep < 2; rep++) {
                        long t0 = System.nanoTime();
                        for (int i = 0; i < n; i++) {
                            bb.clear();
                            cb.clear();
                            dec.reset();
                            dec.decode(bb, cb, true);
                            sink += cb.position();
                        }
                        long dt = System.nanoTime() - t0;
                        if (rep == 1) System.out.println(row + " " + n + " " + (dt / n) + " ns/op");
                    }
                    break;
                }
                case "encode": {
                    CharsetEncoder enc = StandardCharsets.UTF_8.newEncoder();
                    CharBuffer cb = CharBuffer.wrap("0123456789abcdef");
                    ByteBuffer bb = ByteBuffer.allocate(16);
                    for (int rep = 0; rep < 2; rep++) {
                        long t0 = System.nanoTime();
                        for (int i = 0; i < n; i++) {
                            cb.clear();
                            bb.clear();
                            enc.reset();
                            enc.encode(cb, bb, true);
                            sink += bb.position();
                        }
                        long dt = System.nanoTime() - t0;
                        if (rep == 1) System.out.println(row + " " + n + " " + (dt / n) + " ns/op");
                    }
                    break;
                }
                default:
                    throw new IllegalArgumentException(row);
            }
            if (sink == 42) System.out.println("unlikely");
        }
    }

    public static void main(String[] args) {
        String mode = args.length > 0 ? args[0] : "conf";
        if (mode.equals("rows")) {
            rows(System.getProperty("nio21.rows", "decode,encode"));
        } else {
            conf();
        }
    }
}
