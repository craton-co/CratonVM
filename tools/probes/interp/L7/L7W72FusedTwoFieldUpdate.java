// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 72, lane L7: the two-field update
// `aload_N; aload_N; getfield #G; <operand>; iadd | isub; [i2b | i2c | i2s];
// putfield #F` with #F != #G (`this.a = this.b + k`), under
// CRATONVM_INTERP_FUSED_FIELD_UPDATE_TWO_FIELDS (default off; admitted only
// with CRATONVM_INTERP_FUSED_FIELD_INCREMENT_NO_DUP, default off, on top of
// the default-on CRATONVM_INTERP_FUSED_FIELD_INCREMENT and _WIDE). Proposal
// i71-L7-proposal-fuse-the-two-field-update-20261104.
// No behaviour change: every line is the same with each switch off and on,
// with and without --nojit, and with CRATONVM_JIT_NO_FIELD_FAST_PATH=1.
//
// Shapes: int `a = b + 1` / `a = b - k` (iload) / `a = b + 300` (sipush);
// narrow destinations (i2b / i2c / i2s, wrapping); long `la = lb + 1` /
// `la = lb - k` (lload); never fused: an int read into a long store (`i2l`
// between); fused as well: a narrow read into an int store (the read
// site gives the width), a final destination stored in
// the constructor (admitted there); declined: a receiver of a subclass at the same
// site (the site's class check declines); a null receiver after 100 good
// ones (the NPE is the getfield's, on the statement's line).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// int 42 36 341
// narrow -115 65535 -32728
// long 4294967296 4294967292
// mixed 42 narrow read -114
// final 10
// poly 5050 pa=99 qa=100
// npe java.lang.NullPointerException after 100 at npe:114
// HOTSPOT_EXPECTED_END
public class L7W72FusedTwoFieldUpdate {
    int a;
    int b = 41;
    byte nb;
    char nc;
    short ns;
    long la;
    long lb = 0xFFFF_FFFFL;
    int src = 9;
    final int fa;

    L7W72FusedTwoFieldUpdate() {
        fa = src + 1;
    }

    static class P {
        int pa;
        int pb = 1;
    }

    static class Q extends P {
        int qx;
    }

    void ints() {
        for (int i = 0; i < 1000; i++) {
            a = b + 1;
        }
        int r1 = a;
        int k = 5;
        for (int i = 0; i < 1000; i++) {
            a = b - k;
        }
        int r2 = a;
        a = b + 300;
        System.out.println("int " + r1 + " " + r2 + " " + a);
    }

    void narrow() {
        for (int i = 0; i < 10; i++) {
            nb = (byte) (b + 100);
            nc = (char) (b - 42);
            ns = (short) (b + 32767);
        }
        System.out.println("narrow " + nb + " " + (int) nc + " " + ns);
    }

    void longs() {
        la = lb + 1;
        long r1 = la;
        long k = 3;
        la = lb - k;
        System.out.println("long " + r1 + " " + la);
    }

    void mixed() {
        la = b + 1;
        long r1 = la;
        a = nb + 1;
        System.out.println("mixed " + r1 + " narrow read " + a);
    }

    static int poly() {
        P x = null;
        P p = new P();
        P q = new Q();
        int s = 0;
        for (int i = 0; i < 100; i++) {
            x = (i & 1) == 0 ? p : q;
            x.pa = x.pb + i;
            s += x.pa;
        }
        System.out.println("poly " + s + " pa=" + p.pa + " qa=" + q.pa);
        return s;
    }

    static void npe(L7W72FusedTwoFieldUpdate t) {
        L7W72FusedTwoFieldUpdate o = t;
        int done = 0;
        try {
            for (int i = 0; i < 200; i++) {
                o = i == 100 ? null : t;
                o.a = o.b + 1;
                done++;
            }
            System.out.println("npe none");
        } catch (NullPointerException e) {
            StackTraceElement frame = e.getStackTrace()[0];
            System.out.println("npe " + e.getClass().getName() + " after " + done + " at "
                    + frame.getMethodName() + ":" + frame.getLineNumber());
        }
    }

    public static void main(String[] args) {
        L7W72FusedTwoFieldUpdate t = new L7W72FusedTwoFieldUpdate();
        t.ints();
        t.narrow();
        t.longs();
        t.mixed();
        System.out.println("final " + t.fa);
        poly();
        npe(t);
    }
}
