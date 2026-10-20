// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// nio21 wave 4, lane L5: `UTF_8$Encoder.encodeBufferLoop`, the loop every
// encode of an array-less CharBuffer runs (`CharBuffer.wrap(CharSequence)`,
// read-only and direct char buffers). Its `src.get()` / `dst.put(b)` are
// spliced behind a class test on the PARAMETER (`StringCharBuffer`,
// `HeapByteBuffer`), and the optimizing tier now answers that test once, by a
// loop-entry guard that deopts to the loop header (`CRATONVM_JIT_IR_LOOP_CLASS_PREDICATE`),
// cutting the miss arms' calls out of the loop. Switches:
//   CRATONVM_JIT_IR_LOOP_CLASS_PREDICATE=0            in-loop class tests (control arm)
//   CRATONVM_JIT_IR_LOOP_FIELD_FORWARD_PREDICATED=0   no field forward in the guarded loop
//
//   conf   deterministic text, to diff against HotSpot 25 (both modes): the
//          coder result (UNDERFLOW / OVERFLOW / MALFORMED[n]) or the exception
//          class, the source and destination positions and a hash of the
//          bytes written, for ASCII, 2-, 3- and 4-byte text, lone and
//          mismatched surrogates and the empty string, over five source
//          kinds (String, StringBuilder, read-only heap, direct, an offset
//          window) and four destination kinds (heap, direct, read-only, an
//          offset slice) at exact, short-by-one, one-byte and roomy sizes.
//          The table runs cold, then after a warm-up on the String/heap shape
//          only (so the compiled loop guards exactly that pair and every
//          other kind fails the loop-entry guard), then again after a
//          warm-up on every kind. All three passes must print the same lines.
//   rows   `<row> <N> ns/op` lines (one op = one 4 KiB-char encode):
//          utf8-encode    StandardCharsets.UTF_8.encode(CharBuffer.wrap(s)),
//                         ASCII with a 2-, 3- and 4-byte char every 64 chars
//                         (the R17NioRows utfencode shape)
//          buffer-ascii   CharsetEncoder.encode(wrap(ascii), heap, true) into a
//                         reused output (the loop alone, no allocation)
//          buffer-mixed   the same over the mixed text
//
//   java Nio21EncodeLoop conf
//   java -Dnio21.rows=utf8-encode,buffer-ascii,buffer-mixed Nio21EncodeLoop rows

import java.nio.ByteBuffer;
import java.nio.CharBuffer;
import java.nio.charset.CharsetEncoder;
import java.nio.charset.CoderResult;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.List;

public class Nio21EncodeLoop {
    static final int HOT = 20_000;
    static volatile long sink;

    public static void main(String[] args) {
        String mode = args.length > 0 ? args[0] : "conf";
        if (mode.equals("rows")) {
            rows(System.getProperty("nio21.rows", "utf8-encode,buffer-ascii,buffer-mixed"));
        } else {
            conf();
        }
    }

    // ------------------------------------------------------------- inputs

    static String[] texts() {
        String ascii = "The quick brown fox jumps over the lazy dog 0123456789";
        String two = "été жизнь üß";
        String three = "€ 中文 あい";
        String four = "😀 a 🎉 b";
        return new String[] {
            ascii,
            two,
            three,
            four,
            ascii + two + three + four + ascii,
            "",
            // A lone high surrogate at the end, a lone low one in the middle,
            // and a high one followed by a non-surrogate.
            "ab\ud83d",
            "ab\ude00cd",
            "ab\ud83dcd",
        };
    }

    static final String[] SRC_KINDS = {"string", "builder", "readonly", "direct", "window"};
    static final String[] DST_KINDS = {"heap", "direct", "readonly", "slice"};

    static CharBuffer source(String kind, String s) {
        switch (kind) {
            case "string":
                return CharBuffer.wrap(s);
            case "builder":
                return CharBuffer.wrap(new StringBuilder(s));
            case "readonly":
                return CharBuffer.wrap(s.toCharArray()).asReadOnlyBuffer();
            case "direct": {
                CharBuffer cb = ByteBuffer.allocateDirect(2 * s.length() + 2).asCharBuffer();
                cb.put(s);
                cb.flip();
                return cb;
            }
            default: {
                // A window that starts past position 0 of a longer String.
                String padded = "<<" + s + ">>>";
                return CharBuffer.wrap(padded, 2, 2 + s.length());
            }
        }
    }

    static ByteBuffer dest(String kind, int size) {
        switch (kind) {
            case "heap":
                return ByteBuffer.allocate(size);
            case "direct":
                return ByteBuffer.allocateDirect(size);
            case "readonly":
                return ByteBuffer.allocate(size).asReadOnlyBuffer();
            default: {
                ByteBuffer big = ByteBuffer.allocate(size + 7);
                big.position(5);
                big.limit(5 + size);
                return big.slice();
            }
        }
    }

    // ------------------------------------------------------------- one case

    static String run(CharsetEncoder enc, String srcKind, String text, String dstKind, int size) {
        CharBuffer src = source(srcKind, text);
        ByteBuffer dst = dest(dstKind, size);
        String result;
        try {
            enc.reset();
            CoderResult r = enc.encode(src, dst, true);
            result = r.toString();
        } catch (RuntimeException | java.nio.charset.CoderMalfunctionError e) {
            // `CharsetEncoder.encode` wraps a RuntimeException of the loop
            // (a read-only destination's put) in a CoderMalfunctionError.
            Throwable c = e.getCause();
            result = e.getClass().getName() + (c == null ? "" : "<" + c.getClass().getName());
        }
        long h = 17;
        for (int i = 0; i < dst.position(); i++) {
            h = 31 * h + dst.get(i);
        }
        return result + " src=" + src.position() + "/" + src.limit()
            + " dst=" + dst.position() + "/" + dst.limit() + " h=" + h;
    }

    static int[] sizes(String text) {
        int exact = text.getBytes(StandardCharsets.UTF_8).length;
        // A lone surrogate encodes to nothing before the malformed result, so
        // `exact` may undercount what the loop wants; the sizes still differ.
        return new int[] {exact + 8, exact, Math.max(exact - 1, 0), 1};
    }

    static List<String> table(CharsetEncoder enc) {
        List<String> out = new ArrayList<>();
        String[] texts = texts();
        for (int t = 0; t < texts.length; t++) {
            for (String sk : SRC_KINDS) {
                for (String dk : DST_KINDS) {
                    for (int size : sizes(texts[t])) {
                        out.add("t" + t + " " + sk + "->" + dk + " n=" + size + ": "
                            + run(enc, sk, texts[t], dk, size));
                    }
                }
            }
        }
        return out;
    }

    static void warm(CharsetEncoder enc, String[] srcKinds, String[] dstKinds) {
        String[] texts = texts();
        for (int k = 0; k < HOT; k++) {
            String text = texts[k % texts.length];
            String sk = srcKinds[k % srcKinds.length];
            String dk = dstKinds[(k / srcKinds.length) % dstKinds.length];
            sink += run(enc, sk, text, dk, 4 * text.length() + 8).length();
        }
    }

    static void conf() {
        CharsetEncoder enc = StandardCharsets.UTF_8.newEncoder();
        List<String> cold = table(enc);
        for (String line : cold) {
            System.out.println("conf cold " + line);
        }
        // The String/heap pair only: the compiled loop guards that pair.
        warm(enc, new String[] {"string"}, new String[] {"heap"});
        List<String> narrow = table(enc);
        // Every kind: the guard has failed for the others, and the next
        // compile keeps (or re-plants) it.
        warm(enc, SRC_KINDS, DST_KINDS);
        warm(enc, new String[] {"string"}, new String[] {"heap"});
        List<String> wide = table(enc);
        int mismatches = 0;
        for (int i = 0; i < cold.size(); i++) {
            if (!cold.get(i).equals(narrow.get(i))) {
                mismatches++;
                System.out.println("conf MISMATCH narrow " + narrow.get(i));
            }
            if (!cold.get(i).equals(wide.get(i))) {
                mismatches++;
                System.out.println("conf MISMATCH wide " + wide.get(i));
            }
        }
        System.out.println("conf cases=" + cold.size() + " mismatches=" + mismatches);
    }

    // ------------------------------------------------------------- rows

    static String benchText(boolean mixed) {
        StringBuilder sb = new StringBuilder(4098);
        for (int i = 0; sb.length() < 4096; i++) {
            if (mixed && i % 64 == 17) {
                sb.append('é');
            } else if (mixed && i % 64 == 33) {
                sb.append('€');
            } else if (mixed && i % 64 == 49) {
                sb.appendCodePoint(0x1F600);
            } else {
                sb.append((char) ('a' + i % 26));
            }
        }
        return sb.toString();
    }

    static void rows(String which) {
        int n = Integer.getInteger("nio21.n", 20_000);
        String mixed = benchText(true);
        String ascii = benchText(false);
        for (String row : which.split(",")) {
            switch (row) {
                case "utf8-encode": {
                    for (int w = 0; w < n / 2 + 2_000; w++) {
                        sink += StandardCharsets.UTF_8.encode(CharBuffer.wrap(mixed)).remaining();
                    }
                    long t0 = System.nanoTime();
                    for (int k = 0; k < n; k++) {
                        sink += StandardCharsets.UTF_8.encode(CharBuffer.wrap(mixed)).remaining();
                    }
                    System.out.println(row + " " + (System.nanoTime() - t0) / n + " ns/op");
                    break;
                }
                case "buffer-ascii":
                case "buffer-mixed": {
                    String s = row.equals("buffer-ascii") ? ascii : mixed;
                    CharsetEncoder enc = StandardCharsets.UTF_8.newEncoder();
                    ByteBuffer out = ByteBuffer.allocate(4 * s.length() + 8);
                    for (int w = 0; w < n / 2 + 2_000; w++) {
                        out.clear();
                        enc.reset();
                        enc.encode(CharBuffer.wrap(s), out, true);
                        sink += out.position();
                    }
                    long t0 = System.nanoTime();
                    for (int k = 0; k < n; k++) {
                        out.clear();
                        enc.reset();
                        enc.encode(CharBuffer.wrap(s), out, true);
                        sink += out.position();
                    }
                    System.out.println(row + " " + (System.nanoTime() - t0) / n + " ns/op");
                    break;
                }
                default:
                    System.out.println(row + " unknown");
            }
        }
    }
}
