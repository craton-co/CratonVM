// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// nio21 wave 15, lane L3: a hot method whose `new` names a class that has
// NEVER been loaded when the method compiles. The optimizing tier used to
// refuse the whole method there (`IrBuilder::build` bailed at the `0xbb` arm);
// it now builds the site as `Op::NewCp`, a call of the CP-indexed
// `new_object_cp` helper that resolves, initialises and allocates exactly as
// the interpreter's `new` does (`CRATONVM_JIT_IR_DEFERRED_NEW`, default ON).
// This probe makes each compiled site run for the FIRST time after the
// compile, so the helper's resolution, `<clinit>` and failure edge are what
// answer. Deterministic, conformance only (diff against HotSpot 25, both
// modes, and with CRATONVM_JIT_IR_DEFERRED_NEW=0):
//
//   cold    a user class with a printing `<clinit>`: the first `new` after the
//           warm-up runs it once, the value comes back, a second call does not
//           re-run it
//   bad     a user class whose `<clinit>` throws: ExceptionInInitializerError
//           (cause class and message) on the first compiled `new`,
//           NoClassDefFoundError on the second -- class names and the
//           NoClassDefFoundError message only
//   encode  UTF_8$Encoder.encodeBufferLoop warmed on ASCII through a
//           StringCharBuffer (its `new Surrogate.Parser()` never runs), then a
//           fresh encoder over a surrogate pair, a lone high surrogate and a
//           lone low one: result, positions, bytes in hex
//
//   java Nio21DeferredNew            (all three)
//   java Nio21DeferredNew encode     (one)

import java.nio.ByteBuffer;
import java.nio.CharBuffer;
import java.nio.charset.CharsetEncoder;
import java.nio.charset.CoderResult;
import java.nio.charset.StandardCharsets;

public class Nio21DeferredNew {
    static final int HOT = 200_000;
    static volatile long sink;

    static final class Cold {
        static int inits;
        static {
            inits++;
            System.out.println("Cold.<clinit> ran");
        }
        int v = 7;
    }

    static final class Bad {
        static final int X = boom();
        static int boom() {
            throw new IllegalStateException("bad clinit");
        }
        int w = X;
    }

    static int hot(int i, boolean cold) {
        int s = i & 7;
        if (cold) {
            Cold c = new Cold();
            s += c.v;
        }
        return s;
    }

    static int hotBad(int i, boolean cold) {
        int s = i & 3;
        if (cold) {
            Bad b = new Bad();
            s += b.w;
        }
        return s;
    }

    public static void main(String[] args) {
        String only = args.length > 0 ? args[0] : "all";
        if (only.equals("all") || only.equals("cold")) cold();
        if (only.equals("all") || only.equals("bad")) bad();
        if (only.equals("all") || only.equals("encode")) encode();
    }

    static void cold() {
        long s = 0;
        for (int i = 0; i < HOT; i++) s += hot(i, false);
        sink = s;
        System.out.println("cold warm " + s);
        System.out.println("cold first " + hot(3, true) + " inits=" + Cold.inits);
        System.out.println("cold second " + hot(4, true) + " inits=" + Cold.inits);
        long t = 0;
        for (int i = 0; i < HOT; i++) t += hot(i, (i & 1023) == 0);
        System.out.println("cold mixed " + t + " inits=" + Cold.inits);
    }

    static void bad() {
        long s = 0;
        for (int i = 0; i < HOT; i++) s += hotBad(i, false);
        sink = s;
        System.out.println("bad warm " + s);
        for (int k = 0; k < 3; k++) {
            try {
                System.out.println("bad call " + k + " returned " + hotBad(1, true));
            } catch (Throwable t) {
                Throwable c = t.getCause();
                String line = "bad call " + k + " threw " + t.getClass().getName();
                if (t instanceof ExceptionInInitializerError && c != null) {
                    line += " cause " + c.getClass().getName() + ": " + c.getMessage();
                } else if (t instanceof NoClassDefFoundError) {
                    line += ": " + t.getMessage();
                }
                System.out.println(line);
            }
        }
        long t = 0;
        for (int i = 0; i < HOT; i++) t += hotBad(i, false);
        System.out.println("bad after " + t);
    }

    static String hex(ByteBuffer b) {
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < b.position(); i++) {
            if (i > 0) sb.append(' ');
            sb.append(String.format("%02x", b.get(i) & 0xff));
        }
        return sb.toString();
    }

    static String run(CharsetEncoder enc, String text) {
        enc.reset();
        CharBuffer src = CharBuffer.wrap((CharSequence) text);
        ByteBuffer dst = ByteBuffer.allocate(32);
        CoderResult r = enc.encode(src, dst, true);
        return r + " src=" + src.position() + " dst=" + dst.position() + " [" + hex(dst) + "]";
    }

    static void encode() {
        CharsetEncoder warm = StandardCharsets.UTF_8.newEncoder();
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < 256; i++) sb.append((char) ('a' + i % 26));
        String ascii = sb.toString();
        ByteBuffer out = ByteBuffer.allocate(512);
        long s = 0;
        for (int i = 0; i < HOT / 10; i++) {
            warm.reset();
            out.clear();
            warm.encode(CharBuffer.wrap((CharSequence) ascii), out, true);
            s += out.position();
        }
        sink = s;
        System.out.println("encode warm " + s);
        String[] texts = {
            "a😀b",
            "x\uD800y",
            "p\uDC00q",
            "é€𐐷",
        };
        for (String text : texts) {
            CharsetEncoder fresh = StandardCharsets.UTF_8.newEncoder();
            StringBuilder esc = new StringBuilder();
            for (int i = 0; i < text.length(); i++) {
                esc.append(String.format("\\u%04x", (int) text.charAt(i)));
            }
            System.out.println("encode " + esc + " " + run(fresh, text));
            // The same encoder again: its `sgp` is set now.
            System.out.println("encode again " + esc + " " + run(fresh, text));
        }
    }
}
