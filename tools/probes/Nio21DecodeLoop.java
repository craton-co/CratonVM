// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// nio21 wave 3, lane L5: `UTF_8$Decoder.decodeArrayLoop`'s per-byte loop
// (a non-counted `while (sp < sl)` whose `sp` / `dp` advance by 1..4) now
// gets loop-entry guards `sl <= sa.length`, `dl <= da.length`, `0 <= sp`,
// `0 <= dp`, `0 <= dl` and keeps no null / bounds check on `sa[sp]` and the
// ASCII arm's `da[dp]`. Switches:
//   CRATONVM_JIT_IR_MONOTONE_PREDICATE=0   no pre-header guards (control arm)
//   CRATONVM_JIT_IR_BCE_MONOTONE=0         guards planted, nothing reads them
//
//   conf   deterministic text, to diff against HotSpot 25 (both modes): every
//          decoder result (UNDERFLOW / OVERFLOW / MALFORMED[n]), the source
//          and destination positions and a hash of the chars, for ASCII,
//          2-, 3- and 4-byte text, malformed and truncated sequences, over
//          buffers with a non-zero array offset, exact, short-by-one and
//          roomy outputs -- each case run hot (compiled) and cold, and the
//          two must agree.
//   rows   `<row> <N> ns/op` lines (one op = one 4 KiB decode):
//          asciitail   one leading 2-byte char, then ASCII (the bulk prefix
//                      stops at once, so the loop runs every byte)
//          mixed       ASCII with a 2-, 3- or 4-byte char every 16 bytes
//          cyrillic    2-byte text
//
//   java Nio21DecodeLoop conf
//   java -Dnio21.rows=asciitail,mixed,cyrillic Nio21DecodeLoop rows

import java.nio.ByteBuffer;
import java.nio.CharBuffer;
import java.nio.charset.CharsetDecoder;
import java.nio.charset.CoderResult;
import java.nio.charset.StandardCharsets;

public class Nio21DecodeLoop {
    static final int HOT = 3_000;

    public static void main(String[] args) {
        String mode = args.length > 0 ? args[0] : "conf";
        if (mode.equals("rows")) {
            rows(System.getProperty("nio21.rows", "asciitail,mixed,cyrillic"));
        } else {
            conf();
        }
    }

    // ------------------------------------------------------------- inputs

    static byte[] utf8(String s) {
        return s.getBytes(StandardCharsets.UTF_8);
    }

    static byte[] concat(byte[]... parts) {
        int n = 0;
        for (byte[] p : parts) n += p.length;
        byte[] out = new byte[n];
        int at = 0;
        for (byte[] p : parts) {
            System.arraycopy(p, 0, out, at, p.length);
            at += p.length;
        }
        return out;
    }

    static byte[][] inputs() {
        String ascii = "The quick brown fox jumps over the lazy dog 0123456789";
        String two = "été жизнь üß";
        String three = "€ 中文 あい";
        String four = "😀 a 🎉 b";
        return new byte[][] {
            utf8(ascii),
            utf8("é" + ascii),
            utf8(two),
            utf8(three),
            utf8(four),
            utf8(ascii + two + three + four + ascii),
            // Malformed: a lone continuation byte, an overlong 2-byte form, a
            // surrogate encoded in 3 bytes, a 4-byte lead past U+10FFFF.
            concat(utf8("ab"), new byte[] {(byte) 0x80}, utf8("cd")),
            concat(utf8("ab"), new byte[] {(byte) 0xC0, (byte) 0x80}, utf8("cd")),
            concat(utf8("ab"), new byte[] {(byte) 0xED, (byte) 0xA0, (byte) 0x80}, utf8("cd")),
            concat(utf8("ab"), new byte[] {(byte) 0xF5, (byte) 0x80, (byte) 0x80, (byte) 0x80}),
            // Truncated at the end: a 2-, 3- and 4-byte lead with too few
            // continuation bytes (UNDERFLOW with the lead left unread).
            concat(utf8("xyz"), new byte[] {(byte) 0xC3}),
            concat(utf8("xyz"), new byte[] {(byte) 0xE2, (byte) 0x82}),
            concat(utf8("xyz"), new byte[] {(byte) 0xF0, (byte) 0x9F, (byte) 0x98}),
        };
    }

    // ------------------------------------------------------------- kernel

    /** One decode into a fresh heap buffer pair; the answer as one line. */
    static String decodeOnce(CharsetDecoder dec, byte[] in, int srcPad, int dstPad, int dstLen) {
        byte[] src = new byte[srcPad + in.length + 3];
        System.arraycopy(in, 0, src, srcPad, in.length);
        ByteBuffer bb = ByteBuffer.wrap(src, srcPad, in.length).slice();
        char[] dst = new char[dstPad + dstLen + 2];
        CharBuffer cb = CharBuffer.wrap(dst, dstPad, dstLen).slice();
        dec.reset();
        CoderResult r = dec.decode(bb, cb, true);
        int h = 0;
        for (int i = 0; i < cb.position(); i++) h = 31 * h + cb.get(i);
        return r + " src=" + bb.position() + " dst=" + cb.position() + " h=" + h;
    }

    static String hotThenCold(byte[] in, int srcPad, int dstPad, int dstLen) {
        CharsetDecoder dec = StandardCharsets.UTF_8.newDecoder();
        String cold = decodeOnce(dec, in, srcPad, dstPad, dstLen);
        String hot = cold;
        for (int i = 0; i < HOT; i++) {
            hot = decodeOnce(dec, in, srcPad, dstPad, dstLen);
        }
        return hot.equals(cold) ? hot : ("MISMATCH cold=" + cold + " hot=" + hot);
    }

    static void conf() {
        byte[][] ins = inputs();
        int lines = 0;
        for (int i = 0; i < ins.length; i++) {
            byte[] in = ins[i];
            String full = new String(in, StandardCharsets.UTF_8);
            int need = full.length();
            int[][] shapes = {
                {0, 0, need + 8}, // roomy
                {5, 3, need}, // exact, both buffers offset
                {1, 7, Math.max(0, need - 1)}, // short by one
                {2, 0, 1}, // one char of room
            };
            for (int[] s : shapes) {
                System.out.println("case " + i + " pad=" + s[0] + "/" + s[1] + " cap=" + s[2] + ": "
                        + hotThenCold(in, s[0], s[1], s[2]));
                lines++;
            }
        }
        System.out.println("conf lines " + lines);
    }

    // ------------------------------------------------------------- rows

    static byte[] rowInput(String row) {
        StringBuilder sb = new StringBuilder();
        switch (row) {
            case "asciitail":
                sb.append('é');
                while (sb.length() < 4095) sb.append((char) ('a' + sb.length() % 26));
                break;
            case "mixed": {
                String[] wide = {"é", "€", "😀"};
                int k = 0;
                while (utf8(sb.toString()).length < 4080) {
                    sb.append("abcdefghijklmno");
                    sb.append(wide[k++ % wide.length]);
                }
                break;
            }
            case "cyrillic":
                while (sb.length() < 2048) sb.append((char) (0x0430 + sb.length() % 32));
                break;
            default:
                throw new IllegalArgumentException(row);
        }
        return utf8(sb.toString());
    }

    static void rows(String spec) {
        for (String row : spec.split(",")) {
            byte[] in = rowInput(row);
            CharsetDecoder dec = StandardCharsets.UTF_8.newDecoder();
            ByteBuffer bb = ByteBuffer.wrap(in);
            CharBuffer cb = CharBuffer.allocate(in.length);
            long sink = 0;
            int n = 20_000;
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
                if (rep == 1) {
                    System.out.println(row + " " + n + " " + (dt / n) + " ns/op");
                }
            }
            if (sink == 42) System.out.println("unlikely");
        }
    }
}
