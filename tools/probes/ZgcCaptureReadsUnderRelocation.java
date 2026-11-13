// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company
//
// Lambda-proxy captures are legacy 16-byte cells; with ZGC's load barrier
// armed (the default concurrent relocation profile) compiled code reads a
// reference capture inline and decodes its payload word like a compact
// slot. This keeps a population of lambdas capturing two objects each, calls
// them hot (so the reads run compiled), and churns the heap so concurrent
// cycles relocate the captured objects underneath. Every answer is checked
// against the same arithmetic on the boxes themselves.
//
// Usage: ZgcCaptureReadsUnderRelocation [rounds=60]
// Prints deterministic `fact` lines (equal on HotSpot 25) and ZCR-PASS / ZCR-FAIL.
public class ZgcCaptureReadsUnderRelocation {
    static final class Box {
        final int v;
        final String s;
        Box(int v) {
            this.v = v;
            this.s = "b" + v;
        }
    }

    interface F {
        int apply(int x);
    }

    static F make(Box a, Box b) {
        return x -> x + a.v * 3 + b.v + a.s.length() - b.s.length();
    }

    static int oracle(Box a, Box b, int x) {
        return x + a.v * 3 + b.v + a.s.length() - b.s.length();
    }

    public static void main(String[] args) {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 60;
        int n = 4096;
        Box[] as = new Box[n];
        Box[] bs = new Box[n];
        F[] fs = new F[n];
        for (int i = 0; i < n; i++) {
            as[i] = new Box(i);
            bs[i] = new Box(i * 7 + 1);
            fs[i] = make(as[i], bs[i]);
        }
        long bad = 0;
        long sum = 0;
        Object[] churn = new Object[256];
        for (int r = 0; r < rounds; r++) {
            // Replace a slice each round: new captures, old ones die.
            for (int i = r % 16; i < n; i += 16) {
                as[i] = new Box(i + r);
                bs[i] = new Box(i * 7 + r);
                fs[i] = make(as[i], bs[i]);
            }
            for (int k = 0; k < 40; k++) {
                for (int i = 0; i < n; i++) {
                    int got = fs[i].apply(k);
                    if (got != oracle(as[i], bs[i], k)) bad++;
                    sum += got;
                }
                for (int c = 0; c < churn.length; c++) {
                    churn[c] = new byte[1024 + (c & 63)];
                }
            }
        }
        System.out.println("fact sum " + sum);
        System.out.println("fact mismatches " + bad);
        System.out.println(bad == 0 ? "ZCR-PASS" : "ZCR-FAIL");
    }
}
