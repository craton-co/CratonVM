// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 72, lane L7: the fused array element increment,
// `aload_N; iload_M; dup2; <x>aload; <operand>; iadd | isub | imul | iand |
// ior | ixor; [i2b | i2c | i2s]; <x>astore` (and the `laload` / `ladd`
// twins), under CRATONVM_INTERP_FUSED_ARRAY_INCREMENT; its value forms
// (`dup_x2` / `dup2_x2`: `x = a[j]++`, `x = ++a[j]`) under
// CRATONVM_INTERP_FUSED_ARRAY_INCREMENT_VALUE; and the group whose array is
// already on the stack (`this.counts[j]++`, `m[r][c] += k`) under
// CRATONVM_INTERP_FUSED_ARRAY_INCREMENT_TOP, and the group entered at its
// `dup2` however the array and index were computed (`h[i & 3]++`,
// `cnt[s.charAt(i)] += 2`) under CRATONVM_INTERP_FUSED_ARRAY_INCREMENT_DUP2.
// All four default off. Proposal
// i71-L7-proposal-fuse-the-array-element-increment-20261104.
// No behaviour change: every line is the same with each switch off and on
// (the value and top switches only with the base one), with and without
// --nojit, and with CRATONVM_JIT_NO_FIELD_FAST_PATH=1.
//
// Every index is a local in slots 0..3 where the fused shape needs it (the
// loops assign `j` before the group), so the fused path is what runs on the
// warm iterations; the exception lines check that a decline lets the
// unfused `*aload` throw at its own line with HotSpot's message.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// int 334 333 333 wrap -2147483647
// ops -70 243 240 255 255 shift 1024 div 12
// byte 44 44 -1
// char 65535 14464
// short -32768 32767
// long 4294967296 4294967301 4294967300 -9223372036854775808
// value int x=41 a=42 y=43 a=43
// value byte bx=127 b=-128 by=-127
// value char cx=65535 c=65535
// value long lx=9223372036854775807 l=-9223372036854775808 ly=-9223372036854775807
// value loop sum=499500 last=999 pre=500500
// top counts 125 125 125 125 125 125 125 125 used=896
// top matrix 1500 500 rows 2 used=1
// top long 30 used=20
// declined bool true true float 3.0 double 4.5
// aioobe java.lang.ArrayIndexOutOfBoundsException Index 3 out of bounds for length 3 at aioobeInt:258 a=1,1,1
// aioobe java.lang.ArrayIndexOutOfBoundsException Index -1 out of bounds for length 3 at aioobeInt:258 a=0,0,0
// aioobe java.lang.ArrayIndexOutOfBoundsException Index 2 out of bounds for length 2 at aioobeLong:272 l=1,1
// aioobe java.lang.ArrayIndexOutOfBoundsException Index 8 out of bounds for length 8 at aioobeTop:284 counts=1
// npe java.lang.NullPointerException after 100 at npeLocal:299
// npe java.lang.NullPointerException after 100 at npeTop:318
// npe java.lang.NullPointerException after 100 at npeValue:338 sum=4950
// computed 250 250 l=6 o=4 long -5 -5
// HOTSPOT_EXPECTED_END
public class L7W72FusedArrayIncrement {
    int[] counts = new int[8];
    long[] lcounts = new long[2];
    int[][] m = new int[2][2];

    static void ints() {
        int[] a = new int[3];
        int j = 0;
        for (int i = 0; i < 1000; i++) {
            j = i % 3;
            a[j]++;
        }
        int a0 = a[0];
        a[0] = Integer.MAX_VALUE - 1;
        j = 0;
        for (int i = 0; i < 3; i++) {
            a[j]++;
        }
        System.out.println("int " + a0 + " " + a[1] + " " + a[2] + " wrap " + a[0]);
    }

    static void ops() {
        int[] a = new int[6];
        int j = 0;
        int k = 3;
        a[1] = 1;
        a[2] = 0x1FF;
        a[3] = 0xF0;
        a[4] = 0xFF;
        for (int i = 0; i < 10; i++) {
            a[j] -= 7;
        }
        j = 1;
        for (int i = 0; i < 5; i++) {
            a[j] *= k;
        }
        j = 2;
        a[j] &= 0xF0;
        j = 3;
        a[j] |= 0x0F;
        j = 4;
        for (int i = 0; i < 2; i++) {
            a[j] ^= 0x55;
        }
        j = 5;
        a[j] = 1;
        for (int i = 0; i < 10; i++) {
            a[j] <<= 1;
        }
        int[] d = {100};
        j = 0;
        d[j] /= 8;
        System.out.println("ops " + a[0] + " " + a[1] + " " + a[2] + " " + a[3] + " " + a[4]
                + " shift " + a[5] + " div " + d[0]);
    }

    static void narrow() {
        byte[] b = new byte[3];
        int j = 0;
        for (int i = 0; i < 300; i++) {
            b[j]++;
        }
        j = 1;
        for (int i = 0; i < 3; i++) {
            b[j] += 100;
        }
        j = 2;
        b[j]--;
        System.out.println("byte " + b[0] + " " + b[1] + " " + b[2]);
        char[] c = new char[2];
        j = 0;
        c[j]--;
        j = 1;
        for (int i = 0; i < 2; i++) {
            c[j] += 40000;
        }
        System.out.println("char " + (int) c[0] + " " + (int) c[1]);
        short[] s = {32767, 0};
        j = 0;
        s[j]++;
        int t = (int) s[j];
        s[j] -= 1;
        System.out.println("short " + t + " " + s[0]);
    }

    static void longs() {
        long[] la = {0xFFFF_FFFFL, Long.MAX_VALUE};
        int j = 0;
        long k = 5;
        la[j]++;
        long first = la[j];
        la[j] += k;
        long second = la[j];
        la[j] -= 1;
        j = 1;
        la[j]++;
        System.out.println("long " + first + " " + second + " " + la[0] + " " + la[1]);
    }

    static void values() {
        int[] a = {41};
        int j = 0;
        int x = a[j]++;
        int xa = a[0];
        int y = ++a[j];
        System.out.println("value int x=" + x + " a=" + xa + " y=" + y + " a=" + a[0]);
        byte[] b = {127};
        int bx = b[j]++;
        int bmid = b[0];
        int by = ++b[j];
        System.out.println("value byte bx=" + bx + " b=" + bmid + " by=" + by);
        char[] c = {0};
        int cx = --c[j];
        System.out.println("value char cx=" + cx + " c=" + (int) c[0]);
        long[] l = {Long.MAX_VALUE};
        long lx = l[j]++;
        long lmid = l[0];
        long ly = ++l[j];
        System.out.println("value long lx=" + lx + " l=" + lmid + " ly=" + ly);
        int[] v = new int[1];
        int sum = 0;
        int last = 0;
        for (int i = 0; i < 1000; i++) {
            last = v[j]++;
            sum += last;
        }
        int[] w = new int[1];
        int pre = 0;
        for (int i = 0; i < 1000; i++) {
            pre += ++w[j];
        }
        System.out.println("value loop sum=" + sum + " last=" + last + " pre=" + pre);
    }

    void topCounts() {
        int j = 0;
        for (int i = 0; i < 1000; i++) {
            j = i & 7;
            counts[j]++;
        }
        int used = 0;
        j = 3;
        for (int i = 0; i < 7; i++) {
            used += counts[j]++;
        }
        counts[j] -= 7;
        StringBuilder sb = new StringBuilder("top counts");
        for (int c : counts) {
            sb.append(' ').append(c);
        }
        System.out.println(sb + " used=" + used);
    }

    void topMatrix() {
        int r = 0;
        int c = 0;
        for (int i = 0; i < 1000; i++) {
            r = (i & 3) == 0 ? 1 : 0;
            m[r][c] += 2;
        }
        int[] row = m[1];
        int rused = ++row[c];
        row[c]--;
        System.out.println("top matrix " + m[0][0] + " " + m[1][0] + " rows " + m.length
                + " used=" + (rused - m[1][0]));
    }

    void topLong() {
        long ku = 10;
        int j = 1;
        lcounts[j] += ku;
        long lu = (lcounts[j] += ku);
        lcounts[j] += ku;
        System.out.println("top long " + lcounts[1] + " used=" + lu);
    }

    static void declined() {
        boolean[] z = new boolean[2];
        int j = 0;
        z[j] |= true;
        j = 1;
        z[j] ^= true;
        float[] f = {1.5f};
        j = 0;
        f[j] += 1.5f;
        double[] d = {3.0};
        d[j] += 1.5;
        System.out.println("declined bool " + z[0] + " " + z[1] + " float " + f[0] + " double "
                + d[0]);
    }

    static void report(String what, Throwable e, String tail) {
        StackTraceElement frame = e.getStackTrace()[0];
        System.out.println(what + " " + e.getClass().getName() + " " + e.getMessage() + " at "
                + frame.getMethodName() + ":" + frame.getLineNumber() + " " + tail);
    }

    static String join(int[] a) {
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < a.length; i++) {
            sb.append(i == 0 ? "" : ",").append(a[i]);
        }
        return sb.toString();
    }

    static void aioobeInt(int[] a, int from, int to) {
        try {
            for (int j = from; j < to; j++) {
                a[j]++;
            }
            System.out.println("aioobe none");
        } catch (ArrayIndexOutOfBoundsException e) {
            report("aioobe", e, "a=" + join(a));
        }
    }

    static void aioobeLong(long[] l, int to) {
        try {
            int j = 0;
            long one = 1;
            for (int i = 0; i < to; i++) {
                j = i;
                l[j] += one;
            }
            System.out.println("aioobe none");
        } catch (ArrayIndexOutOfBoundsException e) {
            report("aioobe", e, "l=" + l[0] + "," + l[1]);
        }
    }

    void aioobeTop() {
        counts = new int[8];
        try {
            for (int j = 7; j < 9; j++) {
                counts[j]++;
            }
            System.out.println("aioobe none");
        } catch (ArrayIndexOutOfBoundsException e) {
            report("aioobe", e, "counts=" + counts[7]);
        }
    }

    static void npeLocal(int[] good) {
        int[] a = good;
        int j = 0;
        int done = 0;
        try {
            for (int i = 0; i < 200; i++) {
                a = i == 100 ? null : good;
                a[j]++;
                done++;
            }
            System.out.println("npe none");
        } catch (NullPointerException e) {
            StackTraceElement frame = e.getStackTrace()[0];
            System.out.println("npe " + e.getClass().getName() + " after " + done + " at "
                    + frame.getMethodName() + ":" + frame.getLineNumber());
        }
    }

    void npeTop() {
        int j = 0;
        int done = 0;
        int[] keep = counts;
        try {
            for (int i = 0; i < 200; i++) {
                counts = i == 100 ? null : keep;
                j = i & 7;
                counts[j]++;
                done++;
            }
            System.out.println("npe none");
        } catch (NullPointerException e) {
            StackTraceElement frame = e.getStackTrace()[0];
            System.out.println("npe " + e.getClass().getName() + " after " + done + " at "
                    + frame.getMethodName() + ":" + frame.getLineNumber());
        }
        counts = keep;
    }

    static void npeValue(int[] good) {
        int[] a = good;
        int j = 0;
        int done = 0;
        int sum = 0;
        try {
            for (int i = 0; i < 200; i++) {
                a = i == 100 ? null : good;
                sum += a[j]++;
                done++;
            }
            System.out.println("npe none");
        } catch (NullPointerException e) {
            StackTraceElement frame = e.getStackTrace()[0];
            System.out.println("npe " + e.getClass().getName() + " after " + done + " at "
                    + frame.getMethodName() + ":" + frame.getLineNumber() + " sum=" + sum);
        }
    }

    static void computed() {
        int[] h = new int[4];
        for (int i = 0; i < 1000; i++) {
            h[i & 3]++;
        }
        int[] cnt = new int[128];
        String s = "hello world";
        for (int i = 0; i < s.length(); i++) {
            cnt[s.charAt(i)] += 2;
        }
        long[] lh = new long[2];
        long one = 1;
        for (int i = 0; i < 10; i++) {
            lh[i & 1] -= one;
        }
        System.out.println("computed " + h[0] + " " + h[3] + " l=" + cnt['l'] + " o=" + cnt['o']
                + " long " + lh[0] + " " + lh[1]);
    }

    public static void main(String[] args) {
        ints();
        ops();
        narrow();
        longs();
        values();
        L7W72FusedArrayIncrement t = new L7W72FusedArrayIncrement();
        t.topCounts();
        t.topMatrix();
        t.topLong();
        declined();
        aioobeInt(new int[3], 0, 5);
        aioobeInt(new int[3], -1, 1);
        aioobeLong(new long[2], 3);
        t.aioobeTop();
        npeLocal(new int[1]);
        t.npeTop();
        npeValue(new int[1]);
        computed();
    }
}
