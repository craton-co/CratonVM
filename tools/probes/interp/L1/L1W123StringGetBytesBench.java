// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 123, lane L1 -- the A/B bench for the
// `JdkStringGetBytes` compat family (alignment bit 95,
// `CRATONVM_COMPAT_JDK_STRING_GET_BYTES`, default off): with the row on,
// String.getBytes(Charset) / getBytes(String) run the JDK bodies instead of
// the `register_real_charset_natives` bridge front. Page:
// docs/internal/proposals/interpreter/i74-L1-proposal-route-to-own-bytecode-natives-become-family-rows-20261107.md.
//
// TIME lines: median ns per call over REPS reps, per row (short ASCII,
// short Latin-1, mixed BMP, 1 KiB ASCII) x (UTF-8, ISO-8859-1, US-ASCII by
// Charset; UTF-8 by name; Shift_JIS by Charset). The compared lines are
// checksums, so the arms must also agree on the bytes.
//
// Run (pin to P-cores on the hybrid host; interleave the arms):
//   javac -d out L1W123StringGetBytesBench.java
//   java -cp out L1W123StringGetBytesBench
//   cratonvm --compatible -cp out L1W123StringGetBytesBench
//   CRATONVM_COMPAT_JDK_STRING_GET_BYTES=1 cratonvm --compatible -cp out L1W123StringGetBytesBench
//   cratonvm --jdk-only -cp out L1W123StringGetBytesBench
//   (each also with --nojit for the interpreter rows)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags and -Xint identical; TIME lines filtered)
// ascii16 utf8-cs len=16 hash=1323497406
// ascii16 latin1-cs len=16 hash=1323497406
// ascii16 ascii-cs len=16 hash=1323497406
// ascii16 utf8-name len=16 hash=1323497406
// ascii16 sjis-cs len=16 hash=1323497406
// latin1-16 utf8-cs len=21 hash=-1628118312
// latin1-16 latin1-cs len=17 hash=-215496344
// latin1-16 ascii-cs len=17 hash=794346542
// latin1-16 utf8-name len=21 hash=-1628118312
// latin1-16 sjis-cs len=17 hash=794346542
// bmp16 utf8-cs len=22 hash=382693421
// bmp16 latin1-cs len=15 hash=-548400613
// bmp16 ascii-cs len=15 hash=-1076769403
// bmp16 utf8-name len=22 hash=382693421
// bmp16 sjis-cs len=17 hash=-1913655471
// ascii1k utf8-cs len=1024 hash=1504227166
// ascii1k latin1-cs len=1024 hash=1504227166
// ascii1k ascii-cs len=1024 hash=1504227166
// ascii1k utf8-name len=1024 hash=1504227166
// ascii1k sjis-cs len=1024 hash=1504227166
// done
// HOTSPOT_EXPECTED_END

import java.nio.charset.Charset;
import java.nio.charset.StandardCharsets;
import java.util.Arrays;

public class L1W123StringGetBytesBench {
    static final int REPS = 7;
    static final int OPS = 20_000;

    static String u(int... units) {
        StringBuilder sb = new StringBuilder(units.length);
        for (int c : units) {
            sb.append((char) c);
        }
        return sb.toString();
    }

    static volatile long sink;

    interface Op {
        byte[] run(String s) throws Exception;
    }

    static void row(String label, String s, Op op) throws Exception {
        // Checksum (compared) from one call; then the timed reps.
        byte[] once = op.run(s);
        System.out.println(label + " len=" + once.length + " hash=" + Arrays.hashCode(once));
        long[] ns = new long[REPS];
        for (int r = 0; r < REPS; r++) {
            long acc = 0;
            long t0 = System.nanoTime();
            for (int i = 0; i < OPS; i++) {
                acc += op.run(s).length;
            }
            ns[r] = System.nanoTime() - t0;
            sink += acc;
        }
        Arrays.sort(ns);
        System.out.println("TIME " + label + " ns/op=" + (ns[REPS / 2] / OPS) + " min=" + (ns[0] / OPS));
    }

    public static void main(String[] args) throws Exception {
        Charset sjis = Charset.forName("Shift_JIS");
        StringBuilder kb = new StringBuilder();
        while (kb.length() < 1024) kb.append("The quick brown fox jumps over the lazy dog. ");
        String[][] inputs = {
            { "ascii16", "Hello, World 012" },
            { "latin1-16", "caf" + u(0xe9) + " na" + u(0xef) + "ve r" + u(0xe9) + "sum" + u(0xe9) },
            { "bmp16", "a" + u(0xe9, 0x20ac, 0x65e5, 0x672c) + "bcdefghijk" },
            { "ascii1k", kb.substring(0, 1024) },
        };
        for (String[] in : inputs) {
            String label = in[0];
            String s = in[1];
            row(label + " utf8-cs", s, x -> x.getBytes(StandardCharsets.UTF_8));
            row(label + " latin1-cs", s, x -> x.getBytes(StandardCharsets.ISO_8859_1));
            row(label + " ascii-cs", s, x -> x.getBytes(StandardCharsets.US_ASCII));
            row(label + " utf8-name", s, x -> x.getBytes("UTF-8"));
            row(label + " sjis-cs", s, x -> x.getBytes(sjis));
        }
        System.out.println("done");
    }
}
