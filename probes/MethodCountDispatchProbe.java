// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Does the cost of ONE instance call depend on how many methods the receiver
// class declares?
//
// InterpInvokeCostProbe measured final-instance at 384ns against
// invokevirtual at 258ns -- an inversion, since a final method cannot be
// overridden and is the CHEAPEST instance call on HotSpot. But those two arms
// used different receiver classes: the final call landed on the 20-method probe
// class, the virtual call on a 1-method class. So that number is consistent
// with two different causes, and the probe could not tell them apart:
//
//   (a) final misses a dispatch cache that virtual hits, or
//   (b) resolution scans the declaring method list linearly, so the cost is a
//       function of CLASS SIZE and finality is irrelevant.
//
// Here finality is held fixed and only the method count varies: One1 and One64
// both expose hot() as a plain non-final instance method, called through the
// same static type. If (b) is right, One64 costs materially more than One1. If
// they are within noise, class size is not the driver.
//
// FinalOne1/FinalOne64 repeat it with final, so both factors can be read
// against each other in a single run rather than across two.
//
// Position is the other half of hypothesis (b): a linear scan that starts at
// the top would find a FIRST-declared method immediately and report no
// difference at all. So One64 declares hot() first and hotLast() last, and both
// are measured.
public final class MethodCountDispatchProbe {

    static final int UNROLL = 16;

    static class One1 { int hot() { return 1; } }
    static class FinalOne1 { final int hot() { return 1; } }

    static class One64 {
        int hot() { return 1; }
        int m01(){return 1;} int m02(){return 1;} int m03(){return 1;} int m04(){return 1;}
        int m05(){return 1;} int m06(){return 1;} int m07(){return 1;} int m08(){return 1;}
        int m09(){return 1;} int m10(){return 1;} int m11(){return 1;} int m12(){return 1;}
        int m13(){return 1;} int m14(){return 1;} int m15(){return 1;} int m16(){return 1;}
        int m17(){return 1;} int m18(){return 1;} int m19(){return 1;} int m20(){return 1;}
        int m21(){return 1;} int m22(){return 1;} int m23(){return 1;} int m24(){return 1;}
        int m25(){return 1;} int m26(){return 1;} int m27(){return 1;} int m28(){return 1;}
        int m29(){return 1;} int m30(){return 1;} int m31(){return 1;} int m32(){return 1;}
        int m33(){return 1;} int m34(){return 1;} int m35(){return 1;} int m36(){return 1;}
        int m37(){return 1;} int m38(){return 1;} int m39(){return 1;} int m40(){return 1;}
        int m41(){return 1;} int m42(){return 1;} int m43(){return 1;} int m44(){return 1;}
        int m45(){return 1;} int m46(){return 1;} int m47(){return 1;} int m48(){return 1;}
        int m49(){return 1;} int m50(){return 1;} int m51(){return 1;} int m52(){return 1;}
        int m53(){return 1;} int m54(){return 1;} int m55(){return 1;} int m56(){return 1;}
        int m57(){return 1;} int m58(){return 1;} int m59(){return 1;} int m60(){return 1;}
        int m61(){return 1;} int m62(){return 1;}
        int hotLast() { return 1; }
    }

    static class FinalOne64 {
        final int hot() { return 1; }
        int m01(){return 1;} int m02(){return 1;} int m03(){return 1;} int m04(){return 1;}
        int m05(){return 1;} int m06(){return 1;} int m07(){return 1;} int m08(){return 1;}
        int m09(){return 1;} int m10(){return 1;} int m11(){return 1;} int m12(){return 1;}
        int m13(){return 1;} int m14(){return 1;} int m15(){return 1;} int m16(){return 1;}
        int m17(){return 1;} int m18(){return 1;} int m19(){return 1;} int m20(){return 1;}
        int m21(){return 1;} int m22(){return 1;} int m23(){return 1;} int m24(){return 1;}
        int m25(){return 1;} int m26(){return 1;} int m27(){return 1;} int m28(){return 1;}
        int m29(){return 1;} int m30(){return 1;} int m31(){return 1;} int m32(){return 1;}
        int m33(){return 1;} int m34(){return 1;} int m35(){return 1;} int m36(){return 1;}
        int m37(){return 1;} int m38(){return 1;} int m39(){return 1;} int m40(){return 1;}
        int m41(){return 1;} int m42(){return 1;} int m43(){return 1;} int m44(){return 1;}
        int m45(){return 1;} int m46(){return 1;} int m47(){return 1;} int m48(){return 1;}
        int m49(){return 1;} int m50(){return 1;} int m51(){return 1;} int m52(){return 1;}
        int m53(){return 1;} int m54(){return 1;} int m55(){return 1;} int m56(){return 1;}
        int m57(){return 1;} int m58(){return 1;} int m59(){return 1;} int m60(){return 1;}
        int m61(){return 1;} int m62(){return 1;}
        final int hotLast() { return 1; }
    }

    static int k1(int n, One1 o){int a=0;for(int i=0;i<n;i++){a+=o.hot();a+=o.hot();a+=o.hot();a+=o.hot();a+=o.hot();a+=o.hot();a+=o.hot();a+=o.hot();a+=o.hot();a+=o.hot();a+=o.hot();a+=o.hot();a+=o.hot();a+=o.hot();a+=o.hot();a+=o.hot();}return a;}
    static int b1(int n, One1 o){int a=0;for(int i=0;i<n;i++){a+=o.hot();}return a;}

    static int k64(int n, One64 o){int a=0;for(int i=0;i<n;i++){a+=o.hot();a+=o.hot();a+=o.hot();a+=o.hot();a+=o.hot();a+=o.hot();a+=o.hot();a+=o.hot();a+=o.hot();a+=o.hot();a+=o.hot();a+=o.hot();a+=o.hot();a+=o.hot();a+=o.hot();a+=o.hot();}return a;}
    static int b64(int n, One64 o){int a=0;for(int i=0;i<n;i++){a+=o.hot();}return a;}

    static int k64L(int n, One64 o){int a=0;for(int i=0;i<n;i++){a+=o.hotLast();a+=o.hotLast();a+=o.hotLast();a+=o.hotLast();a+=o.hotLast();a+=o.hotLast();a+=o.hotLast();a+=o.hotLast();a+=o.hotLast();a+=o.hotLast();a+=o.hotLast();a+=o.hotLast();a+=o.hotLast();a+=o.hotLast();a+=o.hotLast();a+=o.hotLast();}return a;}
    static int b64L(int n, One64 o){int a=0;for(int i=0;i<n;i++){a+=o.hotLast();}return a;}

    static int kf1(int n, FinalOne1 o){int a=0;for(int i=0;i<n;i++){a+=o.hot();a+=o.hot();a+=o.hot();a+=o.hot();a+=o.hot();a+=o.hot();a+=o.hot();a+=o.hot();a+=o.hot();a+=o.hot();a+=o.hot();a+=o.hot();a+=o.hot();a+=o.hot();a+=o.hot();a+=o.hot();}return a;}
    static int bf1(int n, FinalOne1 o){int a=0;for(int i=0;i<n;i++){a+=o.hot();}return a;}

    static int kf64(int n, FinalOne64 o){int a=0;for(int i=0;i<n;i++){a+=o.hot();a+=o.hot();a+=o.hot();a+=o.hot();a+=o.hot();a+=o.hot();a+=o.hot();a+=o.hot();a+=o.hot();a+=o.hot();a+=o.hot();a+=o.hot();a+=o.hot();a+=o.hot();a+=o.hot();a+=o.hot();}return a;}
    static int bf64(int n, FinalOne64 o){int a=0;for(int i=0;i<n;i++){a+=o.hot();}return a;}

    static int ka(int n,int s){int a=0;for(int i=0;i<n;i++){a+=s;a+=s;a+=s;a+=s;a+=s;a+=s;a+=s;a+=s;a+=s;a+=s;a+=s;a+=s;a+=s;a+=s;a+=s;a+=s;}return a;}
    static int ba(int n,int s){int a=0;for(int i=0;i<n;i++){a+=s;}return a;}

    static long t(Runnable r){long x=System.nanoTime();r.run();return System.nanoTime()-x;}

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 120_000;
        One1 o1 = new One1(); One64 o64 = new One64();
        FinalOne1 f1 = new FinalOne1(); FinalOne64 f64 = new FinalOne64();
        int[] g = new int[1];

        for (int w = 0; w < 3; w++) {
            int m = n/10;
            g[0]+=k1(m,o1)+b1(m,o1)+k64(m,o64)+b64(m,o64)+k64L(m,o64)+b64L(m,o64)
                 +kf1(m,f1)+bf1(m,f1)+kf64(m,f64)+bf64(m,f64)+ka(m,3)+ba(m,3);
        }

        // INTERLEAVED, and the reason is a wrong answer this probe already
        // produced. Run once as six sequential arms, it reported final at 3.8x
        // virtual -- on a host whose load was ramping, which showed up as the
        // iadd control reading 61.6ns against 21.9ns in a run minutes earlier.
        // The arms are ordered, so rising load lands entirely on whichever
        // arm is measured last, and "final" was measured after "virtual".
        // Sequential arms cannot separate a real effect from host drift.
        //
        // Each ROUND now measures every arm, and the rounds are summed, so
        // drift spreads across all arms instead of accumulating on the late
        // ones. The per-round control is printed so a run whose control moves
        // can be discarded rather than believed.
        final int ROUNDS = 5;
        long a1=0,c1=0,a64=0,c64=0,aL=0,cL=0,af1=0,cf1=0,af64=0,cf64=0,aa=0,ca=0;
        for (int r = 0; r < ROUNDS; r++) {
            a1  += t(()->g[0]+=k1(n,o1));    c1   += t(()->g[0]+=b1(n,o1));
            af1 += t(()->g[0]+=kf1(n,f1));   cf1  += t(()->g[0]+=bf1(n,f1));
            a64 += t(()->g[0]+=k64(n,o64));  c64  += t(()->g[0]+=b64(n,o64));
            af64+= t(()->g[0]+=kf64(n,f64)); cf64 += t(()->g[0]+=bf64(n,f64));
            aL  += t(()->g[0]+=k64L(n,o64)); cL   += t(()->g[0]+=b64L(n,o64));
            aa  += t(()->g[0]+=ka(n,3));     ca   += t(()->g[0]+=ba(n,3));
            System.out.printf("  round %d control %6.1f ns/op%n", r,
                    (double)(t(()->g[0]+=ka(n,3)) - t(()->g[0]+=ba(n,3)))
                        / ((long) n * (UNROLL - 1)));
        }

        long per = (long) n * (UNROLL - 1) * ROUNDS;
        System.out.printf("virtual  1-method class         %8.1f ns/call%n",(double)(a1-c1)/per);
        System.out.printf("virtual 64-method class (first) %8.1f ns/call%n",(double)(a64-c64)/per);
        System.out.printf("virtual 64-method class (last)  %8.1f ns/call%n",(double)(aL-cL)/per);
        System.out.printf("final    1-method class         %8.1f ns/call%n",(double)(af1-cf1)/per);
        System.out.printf("final   64-method class         %8.1f ns/call%n",(double)(af64-cf64)/per);
        System.out.printf("iadd(ctrl)                      %8.1f ns/op%n",(double)(aa-ca)/per);
        System.out.println("guard "+g[0]);
    }
}
