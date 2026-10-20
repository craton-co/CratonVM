// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 22, lane fuzz8: finalizable classes whose constructors
// use JEP 513 flexible constructor bodies -- statements before `super(..)` /
// `this(..)`, including a try/catch, a loop, field assignments and a throw --
// thrown before and after the initializing call, constructed hot enough to
// compile.
//
// Risk targeted: wave 21 item 8 of r21-review-w21-findings-20261019.md (the
// allocation-free `<init>`-return scan `ctor_body_keeps_receiver_and_calls_before`
// that now accepts JEP 513 prologues with handlers and branches before the
// `invokespecial`), and the finalizer registration rules of waves 13-18
// (`CRATONVM_FINALIZER_REREGISTER_AT_INIT_RETURN`, the ancestor walk). HotSpot
// registers a finalizer when Object.<init> returns (RegisterFinalizersAtInit),
// so an object is finalized exactly when its constructor chain reached
// Object.<init>:
//   mode 0  normal                                   finalized
//   mode 1  Fin's prologue throws before super(..)   NOT finalized
//   mode 2  Fin throws after super(..) returned      finalized
//   mode 3  Base throws after its own super()        finalized
//   mode 4  Base's prologue throws before super()    NOT finalized
// Phase A uses Fin(String, int) directly; phase B uses Fin(int), whose
// prologue loops and then delegates with this(..). Base.<init> is warmed on
// plain Base objects before Fin is first loaded.
//
// Run:
//   javac -d out R21Fz8FinPrologue1.java          # JDK 25 (JEP 513 is final in 25)
//   java -Xmx256m -cp out R21Fz8FinPrologue1                     # HotSpot 25
//   cratonvm -Xmx256m -cp out R21Fz8FinPrologue1                 # --jdk-only
//   cratonvm --compatible -Xmx256m -cp out R21Fz8FinPrologue1
//   cratonvm --nojit -Xmx256m -cp out R21Fz8FinPrologue1
// HotSpot expectation (by reading JEP 513 and HotSpot's
// RegisterFinalizersAtInit / Object.<init> return; not yet run):
//   warm 102399860
//   A made=4000 thrown=16000 pre=4000 post=4000 basePost=4000 basePre=4000 nfe=10000 finalized 4000,0,4000,4000,0 total=12000
//   B made=4000 thrown=16000 pre=4000 post=4000 basePost=4000 basePre=4000 nfe=0 finalized 4000,0,4000,4000,0 total=12000
// A short "finalized" line means the settle loop timed out: rerun before filing.

import java.util.concurrent.atomic.AtomicIntegerArray;

public class R21Fz8FinPrologue1 {
    static final int N = 20_000;
    static final AtomicIntegerArray FIN = new AtomicIntegerArray(5);
    static int nfe;

    static class Base {
        final int v;

        Base(int v, int mode) {
            if (mode == 4) {
                throw new IllegalStateException("base-pre");
            }
            super();
            if (mode == 3) {
                throw new IllegalStateException("base-post");
            }
            this.v = v;
        }
    }

    // Loaded only after Base.<init> is hot (first touched in phase A).
    static class Fin extends Base {
        final int m;
        final int tag;

        Fin(String s, int mode) {
            int x;
            try {
                x = Integer.parseInt(s);
            } catch (NumberFormatException e) {
                nfe++;
                x = -1;
            }
            int acc = 0;
            for (int i = 0; i < (x & 3); i++) {
                acc += i;
            }
            if (mode == 1) {
                throw new IllegalStateException("pre");
            }
            m = mode;
            tag = acc;
            super(x + acc, mode);
            if (mode == 2) {
                throw new IllegalStateException("post");
            }
        }

        Fin(int k) {
            int s = 0;
            for (int i = 0; i < 3; i++) {
                s += i * k;
            }
            this(Integer.toString(s & 1023), k % 5);
        }

        @Override
        @SuppressWarnings("removal")
        protected void finalize() {
            FIN.incrementAndGet(m);
        }
    }

    static void collect() {
        System.gc();
        System.runFinalization();
        try {
            Thread.sleep(2);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
        }
    }

    static int total() {
        int t = 0;
        for (int i = 0; i < 5; i++) {
            t += FIN.get(i);
        }
        return t;
    }

    static int settle() {
        int last = -1;
        int still = 0;
        for (int i = 0; i < 400; i++) {
            collect();
            int now = total();
            if (now == last) {
                still++;
            } else {
                still = 0;
                last = now;
            }
            if (i >= 20 && still >= 10) {
                break;
            }
        }
        return total();
    }

    static String counts() {
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < 5; i++) {
            if (i > 0) {
                sb.append(',');
            }
            sb.append(FIN.get(i));
        }
        return sb.toString();
    }

    static long warm() {
        long s = 0;
        for (int r = 0; r < 200_000; r++) {
            s += new Base(r & 1023, 0).v;
            if ((r & 7) == 0) {
                try {
                    new Base(r, 3 + ((r >> 3) & 1));
                } catch (IllegalStateException e) {
                    s += e.getMessage().length();
                }
            }
        }
        return s;
    }

    static Object makeA(int r) {
        // Every other argument fails to parse inside the prologue's try.
        return new Fin((r & 2) == 0 ? Integer.toString(r) : "x" + r, r % 5);
    }

    static Object makeB(int r) {
        return new Fin(r);
    }

    static void phase(String tag, boolean a) {
        for (int i = 0; i < 5; i++) {
            FIN.set(i, 0);
        }
        nfe = 0;
        int made = 0;
        int pre = 0;
        int post = 0;
        int basePost = 0;
        int basePre = 0;
        for (int r = 0; r < N; r++) {
            try {
                if ((a ? makeA(r) : makeB(r)) != null) {
                    made++;
                }
            } catch (IllegalStateException e) {
                switch (e.getMessage()) {
                    case "pre" -> pre++;
                    case "post" -> post++;
                    case "base-post" -> basePost++;
                    case "base-pre" -> basePre++;
                    default -> made -= 1000000;
                }
            }
        }
        int thrown = pre + post + basePost + basePre;
        int nfeSeen = nfe;
        int total = settle();
        System.out.println(tag + " made=" + made + " thrown=" + thrown + " pre=" + pre + " post=" + post
                + " basePost=" + basePost + " basePre=" + basePre + " nfe=" + nfeSeen
                + " finalized " + counts() + " total=" + total);
    }

    public static void main(String[] args) {
        System.out.println("warm " + warm());
        settle();
        phase("A", true);
        phase("B", false);
    }
}
