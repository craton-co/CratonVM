// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// gci wave i27, lane kd: `multianewarray` shapes and contents, for the rows of
// `CRATONVM_ZGC_MULTIARRAY_TLAB` (read once per VM since i27/kd; it was a
// process latch). The switch only changes where the sub-arrays come from on
// ZGC, so every line must equal HotSpot's on every collector, switch on or off.
//
//   java     -cp out GciKd27MultiArrayProbe 20000
//   cratonvm -XX:+UseZGC -cp out GciKd27MultiArrayProbe 20000
//
// stdout is deterministic; the elapsed time goes to stderr.
public class GciKd27MultiArrayProbe {
    static long fill2(int a, int b) {
        int[][] m = new int[a][b];
        long sum = m.length;
        for (int i = 0; i < a; i++) {
            sum += m[i].length;
            for (int j = 0; j < b; j++) {
                m[i][j] = i * 31 + j;
                sum += m[i][j];
            }
        }
        return sum;
    }

    static long fill3(int a, int b, int c) {
        long[][][] m = new long[a][b][c];
        long sum = 0;
        for (int i = 0; i < a; i++) {
            for (int j = 0; j < b; j++) {
                sum += m[i][j].length;
                for (int k = 0; k < c; k++) {
                    m[i][j][k] = (long) i * 1_000_003L + j * 101L + k;
                    sum ^= m[i][j][k] * 0x9E3779B97F4A7C15L;
                }
            }
        }
        return sum;
    }

    static long objects(int a, int b) {
        String[][] m = new String[a][b];
        long sum = 0;
        for (int i = 0; i < a; i++) {
            for (int j = 0; j < b; j++) {
                sum += (m[i][j] == null) ? 1 : 0;
            }
            m[i][i % b] = "s" + i;
            sum += m[i][i % b].length();
        }
        return sum;
    }

    static String negative(int a) {
        try {
            int[][] m = new int[3][a];
            return "no exception " + m.length;
        } catch (NegativeArraySizeException e) {
            return "NegativeArraySizeException: " + e.getMessage();
        }
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 20_000;
        long t0 = System.nanoTime();
        long s2 = 0;
        long s3 = 0;
        long so = 0;
        for (int r = 0; r < n; r++) {
            s2 += fill2(r % 64, 3);
            s3 += fill3(r % 7 + 1, 5, r % 4);
            so += objects(r % 16 + 1, 4);
        }
        System.out.println("fill2 " + s2);
        System.out.println("fill3 " + s3);
        System.out.println("objects " + so);
        System.out.println(negative(-5));
        System.out.println("PASS");
        System.err.println("[gci-kd27-multiarray] n=" + n + " ms=" + (System.nanoTime() - t0) / 1_000_000);
    }
}
