// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 91, lane L7: what CRATONVM_INTERP_FUSED_ICONST_ARITH
// (`iload_N; {iconst_m1..5 | bipush | sipush}; iadd | isub | imul | ishl |
// ishr | iushr | iand | ior | ixor [; istore_Z]` as one dispatch) must leave
// unchanged:
//   * every operation over the edge values (wrap-around, shift distances
//     masked to five bits, the sign of ishr against iushr, sipush -32768);
//   * the store form (`int a = x + 1;` into locals 1..3: istore_N) next to
//     the push form (locals 4 and up: istore n, and string-concat operands);
//   * fib's `n - 1` / `n - 2` (the push form ahead of an invokestatic), and a
//     warm loop mixing the forms.
// Each `ops` line runs 3000 times first so the method is warm on the fast
// path when the printed run happens.
//
// Run (all must print the block below):
//   cratonvm --java-home <jdk25> --nojit -cp <dir> L7W91IconstArith
//   CRATONVM_INTERP_FUSED_ICONST_ARITH=1 cratonvm --java-home <jdk25> --nojit -cp <dir> L7W91IconstArith
//   CRATONVM_INTERP_FUSED_ICONST_ARITH=1 cratonvm --java-home <jdk25> -cp <dir> L7W91IconstArith
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// ops 0 1 -1 0 0 0 0 0 0 -2 5 1 0 0
// ops 1 2 0 31 1000 2 0 0 1 -1 4 2 -32768 1
// ops -1 0 -2 -31 -1000 -2 -1 15 255 -1 -6 0 32768 -1
// ops 7 8 6 217 7000 14 0 0 7 -1 2 8 -229376 7
// ops 255 256 254 7905 255000 510 15 0 255 -1 250 256 -8355840 255
// ops -256 -255 -257 -7936 -256000 -512 -16 15 0 -2 -251 -255 8388608 -256
// ops 2147483647 -2147483648 2147483646 2147483617 -1000 -2 134217727 7 255 -1 2147483642 -2147483648 32768 2147483647
// ops -2147483648 -2147483647 2147483647 -2147483648 0 0 -134217728 8 0 -2 -2147483643 -2147483647 0 -2147483648
// ops 123456789 123456790 123456788 -467806837 -1097262584 246913578 7716049 0 21 -1 123456784 123456790 427130880 123456789
// fib 6765
// loop 11648816
// HOTSPOT_EXPECTED_END
public class L7W91IconstArith {
    static String ops(int x) {
        int a = x + 1;
        int b = x - 1;
        int c = x * 31;
        int d = x * 1000;
        int e = x << 33;
        int f = x >> 4;
        int g = x >>> 28;
        int h = x & 0xff;
        int i = x | -2;
        int j = x ^ 5;
        int k = x - -1;
        int l = x * -32768;
        int m = x & -1;
        return x + " " + a + " " + b + " " + c + " " + d + " " + e + " " + f + " " + g + " " + h
                + " " + i + " " + j + " " + k + " " + l + " " + m;
    }

    static int fib(int n) {
        return n < 2 ? n : fib(n - 1) + fib(n - 2);
    }

    static int loop(int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            s += (i & 0xff) - 1;
            s ^= i >>> 3;
        }
        return s;
    }

    public static void main(String[] args) {
        int[] xs = {0, 1, -1, 7, 255, -256, Integer.MAX_VALUE, Integer.MIN_VALUE, 123456789};
        for (int x : xs) {
            String last = null;
            for (int r = 0; r < 3000; r++) {
                last = ops(x);
            }
            System.out.println("ops " + last);
        }
        System.out.println("fib " + fib(20));
        System.out.println("loop " + loop(100000));
    }
}
