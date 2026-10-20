// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 116, lane L8: what the three wave-116 switches
// must leave unchanged.
//
//   CRATONVM_INTERP_FUSED_GETFIELD_CHAIN (default off): a quickened getfield
//     hit runs the getfields right after it in the same dispatch.
//   CRATONVM_INTERP_FUSED_NULL_BRANCH_WIDE (default off): `aload n; ifnull |
//     ifnonnull` as one dispatch.
//   CRATONVM_INTERP_LEAN_LOOP_TOP (default off): the dispatch loop's lean
//     loop-top instance (one gate test per bytecode, cached code length).
//
// Rows:
//   chain2 / chain3   `this.a.b.c` and `h.a.b.c` over warm sites (the chain
//                     starts in the `aload_0; getfield` fusion)
//   chainStatic       `S.a.b.c` (`getstatic` head: the chain starts in the
//                     plain `getfield` arm)
//   chainNull <k>     a null at link k of the chain: the NPE (with HotSpot's
//                     helpful message, which names the getfield that threw)
//                     and the line it is reported on
//   chainTypes        every field type read through a chain
//   chainSub          a chain over a subclass receiver
//   wideNull <o>      `o == null` / `o != null` through `aload 4`
//   wideWalk          a list walk with the cursor in local 5, forward and do-while
//   wideTernary       a `goto` lands on the wide `ifnonnull`
//   lean*             a mix the lean loop top must run as before: frame
//                     switches (recursion, calls, returns), exceptions thrown
//                     and caught in a loop, a synchronized method, a switch
//
// Every line must be the same with each switch off and on (and all on),
// --jdk-only and --compatible, JIT and --nojit.
//
// Run: javac -g -d . L8W116FusionSemantics.java
//      java -cp . L8W116FusionSemantics                                  (HotSpot reference)
//      cratonvm --jdk-only --nojit -cp . L8W116FusionSemantics
//      CRATONVM_INTERP_FUSED_GETFIELD_CHAIN=1 CRATONVM_INTERP_FUSED_NULL_BRANCH_WIDE=1 \
//        CRATONVM_INTERP_LEAN_LOOP_TOP=1 cratonvm --jdk-only --nojit -cp . L8W116FusionSemantics
//      (and each switch alone, without --nojit, and with --compatible)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// chain2 61000
// chain3 30000
// chainStatic 50000
// chainNull 0 java.lang.NullPointerException: Cannot read field "a" because "h" is null line=156
// chainNull 1 java.lang.NullPointerException: Cannot read field "b" because "h.a" is null line=156
// chainNull 2 java.lang.NullPointerException: Cannot read field "c" because "h.a.b" is null line=156
// chainNull 3 ok 7
// chainTypes 1 2 3 4 5.5 6.25 true x 9 end
// chainSub 77
// wideNull null 0x1
// wideNull obj 0x2
// wideWalk 1249975000 1249975000
// wideTernary 1 0 0 1
// leanFib 6765
// leanCatch 3334 6666
// leanSync 100000
// leanSwitch 33334 33333 33333
// leanNested 1000000
// HOTSPOT_EXPECTED_END
public class L8W116FusionSemantics {
    static final class Leaf {
        int c;
        byte vb;
        short vs;
        char vc;
        long vl;
        float vf;
        double vd;
        boolean vz;
        String vs2;
        Object vo;
    }

    static final class Mid {
        Leaf b;
    }

    static class Head {
        Mid a;
        int k;
    }

    static final class SubHead extends Head {
        int extra;
    }

    static final class Node {
        final int v;
        Node next;

        Node(int v) {
            this.v = v;
        }
    }

    Mid a;
    int base;

    L8W116FusionSemantics(Mid a) {
        this.a = a;
    }

    // aload_0; getfield a; getfield b; getfield c
    int chain2() {
        return this.a.b.c;
    }

    static int chain3(Head h) {
        return h.a.b.c;
    }

    static Head shared;

    // getstatic; getfield a; getfield b; getfield c: the chain starts in the
    // plain `getfield` arm, not in the `aload_N; getfield` fusion.
    static int chainStatic() {
        return shared.a.b.c;
    }

    static Head build(int c) {
        Leaf l = new Leaf();
        l.c = c;
        Mid m = new Mid();
        m.b = l;
        Head h = new Head();
        h.a = m;
        return h;
    }

    static String chainNull(int k) {
        Head h = build(7);
        if (k == 0) {
            h = null;
        } else if (k == 1) {
            h.a = null;
        } else if (k == 2) {
            h.a.b = null;
        }
        try {
            // Warm the sites first with a good chain so the fused reads are
            // quickened when the null arrives.
            Head good = build(1);
            int s = 0;
            for (int i = 0; i < 2000; i++) {
                s += readChain(good);
            }
            return "ok " + (readChain(h) + s - 2000);
        } catch (NullPointerException e) {
            StackTraceElement top = e.getStackTrace()[0];
            return e + " line=" + top.getLineNumber();
        }
    }

    static int readChain(Head h) {
        return h.a.b.c;
    }

    static String chainTypes(Head h) {
        return h.a.b.vb + " " + h.a.b.vs + " " + (int) h.a.b.vc + " " + h.a.b.vl + " " + h.a.b.vf
                + " " + h.a.b.vd + " " + h.a.b.vz + " " + h.a.b.vs2 + " " + h.a.b.vo + " end";
    }

    // `o` is local 4 (after two longs): aload 4; ifnonnull / ifnull
    static int wideNull(long p0, long p1, Object o) {
        int r = (int) (p0 + p1);
        if (o == null) {
            r |= 0x1;
        }
        if (o != null) {
            r |= 0x2;
        }
        return r;
    }

    static long wideWalkFor(long p0, long p1, Node head) {
        long s = p0 + p1;
        for (Node n = head; n != null; n = n.next) {
            s += n.v;
        }
        return s;
    }

    static long wideWalkDo(long p0, long p1, Node head) {
        long s = p0 + p1;
        Node n = head;
        do {
            s += n.v;
            n = n.next;
        } while (n != null);
        return s;
    }

    static int wideTernary(long p0, long p1, boolean c, Object a, Object b) {
        if ((c ? a : b) == null) {
            return 1 + (int) (p0 + p1);
        }
        return (int) (p0 + p1);
    }

    static int fib(int n) {
        return n < 2 ? n : fib(n - 1) + fib(n - 2);
    }

    static int thrower(int i) {
        if (i % 3 == 0) {
            throw new IllegalStateException("x" + i);
        }
        return i;
    }

    static String leanCatch() {
        int caught = 0;
        int ok = 0;
        for (int i = 0; i < 10_000; i++) {
            try {
                thrower(i);
                ok++;
            } catch (IllegalStateException e) {
                caught++;
            }
        }
        return caught + " " + ok;
    }

    private int counter;

    synchronized void bump() {
        counter++;
    }

    static int leanSwitch(int[] buckets) {
        for (int i = 0; i < 100_000; i++) {
            switch (i % 3) {
                case 0:
                    buckets[0]++;
                    break;
                case 1:
                    buckets[1]++;
                    break;
                default:
                    buckets[2]++;
                    break;
            }
        }
        return buckets[0];
    }

    static int leanNested() {
        int s = 0;
        for (int i = 0; i < 1000; i++) {
            for (int j = 0; j < 1000; j++) {
                s++;
            }
        }
        return s;
    }

    public static void main(String[] args) {
        Leaf leaf = new Leaf();
        leaf.c = 61;
        Mid mid = new Mid();
        mid.b = leaf;
        L8W116FusionSemantics self = new L8W116FusionSemantics(mid);
        int s2 = 0;
        for (int i = 0; i < 1000; i++) {
            s2 += self.chain2();
        }
        System.out.println("chain2 " + s2);
        Head h3 = build(3);
        int s3 = 0;
        for (int i = 0; i < 10_000; i++) {
            s3 += chain3(h3);
        }
        System.out.println("chain3 " + s3);
        shared = build(5);
        int ss = 0;
        for (int i = 0; i < 10_000; i++) {
            ss += chainStatic();
        }
        System.out.println("chainStatic " + ss);
        for (int k = 0; k <= 3; k++) {
            System.out.println("chainNull " + k + " " + chainNull(k));
        }
        Head ht = build(0);
        Leaf lt = ht.a.b;
        lt.vb = 1;
        lt.vs = 2;
        lt.vc = 3;
        lt.vl = 4;
        lt.vf = 5.5f;
        lt.vd = 6.25;
        lt.vz = true;
        lt.vs2 = "x";
        lt.vo = Integer.valueOf(9);
        String types = null;
        for (int i = 0; i < 500; i++) {
            types = chainTypes(ht);
        }
        System.out.println("chainTypes " + types);
        SubHead sh = new SubHead();
        sh.a = build(77).a;
        int sub = 0;
        for (int i = 0; i < 500; i++) {
            sub = chain3(sh);
        }
        System.out.println("chainSub " + sub);

        System.out.println("wideNull null 0x" + Integer.toHexString(wideNull(0L, 0L, null)));
        System.out.println("wideNull obj 0x" + Integer.toHexString(wideNull(0L, 0L, new Object())));
        Node head = null;
        for (int i = 49_999; i >= 0; i--) {
            Node n = new Node(i);
            n.next = head;
            head = n;
        }
        System.out.println("wideWalk " + wideWalkFor(0L, 0L, head) + " " + wideWalkDo(0L, 0L, head));
        Object x = new Object();
        System.out.println("wideTernary " + wideTernary(0L, 0L, true, null, x) + " "
                + wideTernary(0L, 0L, true, x, null) + " " + wideTernary(0L, 0L, false, null, x)
                + " " + wideTernary(0L, 0L, false, x, null));

        System.out.println("leanFib " + fib(20));
        System.out.println("leanCatch " + leanCatch());
        L8W116FusionSemantics sync = new L8W116FusionSemantics(null);
        for (int i = 0; i < 100_000; i++) {
            sync.bump();
        }
        System.out.println("leanSync " + sync.counter);
        int[] buckets = new int[3];
        leanSwitch(buckets);
        System.out.println("leanSwitch " + buckets[0] + " " + buckets[1] + " " + buckets[2]);
        System.out.println("leanNested " + leanNested());
    }
}
