// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 115, lane L8: what the two branch fusions must
// leave unchanged.
//
//   CRATONVM_INTERP_FUSED_ZERO_BRANCH (default off): `iload{_N, n};
//     ifeq | ifne | iflt | ifge | ifgt | ifle` as one dispatch.
//   CRATONVM_INTERP_FUSED_NULL_BRANCH (default off): `aload_N; ifnull |
//     ifnonnull` as one dispatch.
//
// Rows:
//   zero <x>        the six int-against-zero tests of `x` through `iload_0`
//                   (javac emits the inverted `if<cond>` for each `if`)
//   zeroWide <x>    the same through `iload 4` (x past two long parameters)
//   expr            `s += (x != 0) ? 1 : 2`: the fused branch with a value
//                   already below it on the operand stack
//   ternary         `if ((c ? a : b) != 0)`: a `goto` lands on the `ifeq`
//                   of the `iload_2; ifeq` group (a branch into the group)
//   countDown       `while (n != 0)` (forward exit) and `do .. while (n !=
//                   0)` (a backward fused branch), long enough for OSR
//   flagLoop        `while (running)` over a boolean local
//   null <o>        `o == null` / `o != null` through `aload_0`
//   listWalk        `for (n = head; n != null; n = n.next)` and the
//                   `do .. while (n != null)` back edge over 300 000 nodes
//   nullExpr        `s += (o == null) ? 1 : 2` with `s` below on the stack
//   nullTernary     `if ((c ? a : b) == null)`: a branch into the group
//
// Every line must be the same with each switch off and on (and both on),
// --jdk-only and --compatible, JIT and --nojit.
//
// Run: javac -d . L8W115BranchFusion.java
//      java -cp . L8W115BranchFusion                                    (HotSpot reference)
//      cratonvm --jdk-only --nojit -cp . L8W115BranchFusion
//      CRATONVM_INTERP_FUSED_ZERO_BRANCH=1 CRATONVM_INTERP_FUSED_NULL_BRANCH=1 \
//        cratonvm --jdk-only --nojit -cp . L8W115BranchFusion
//      (and each switch alone, without --nojit, and with --compatible)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// zero -2147483648 0x26
// zero -1 0x26
// zero 0 0x29
// zero 1 0x1a
// zero 2147483647 0x1a
// zeroWide -2147483648 0x26
// zeroWide -1 0x26
// zeroWide 0 0x29
// zeroWide 1 0x1a
// zeroWide 2147483647 0x1a
// expr 1338
// ternary 1 0 1 0
// countDown 20000100000 20000100000
// flagLoop 300000
// null null 0x1
// null obj 0x2
// listWalk 44999850000 44999850000 300000
// nullExpr 1501
// nullTernary 1 0 0 1
// HOTSPOT_EXPECTED_END
public class L8W115BranchFusion {
    static final class Node {
        final int v;
        Node next;

        Node(int v) {
            this.v = v;
        }
    }

    // iload_0; ifne / ifeq / ifge / iflt / ifle / ifgt
    static int zero(int x) {
        int r = 0;
        if (x == 0) {
            r |= 0x01;
        }
        if (x != 0) {
            r |= 0x02;
        }
        if (x < 0) {
            r |= 0x04;
        }
        if (x >= 0) {
            r |= 0x08;
        }
        if (x > 0) {
            r |= 0x10;
        }
        if (x <= 0) {
            r |= 0x20;
        }
        return r;
    }

    // `x` is local 4 (two long parameters before it): iload 4; if<cond>.
    static int zeroWide(long a, long b, int x) {
        int r = (int) (a + b);
        if (x == 0) {
            r |= 0x01;
        }
        if (x != 0) {
            r |= 0x02;
        }
        if (x < 0) {
            r |= 0x04;
        }
        if (x >= 0) {
            r |= 0x08;
        }
        if (x > 0) {
            r |= 0x10;
        }
        if (x <= 0) {
            r |= 0x20;
        }
        return r;
    }

    // iload s; iload x; ifeq: the fused branch over a non-empty stack.
    static int expr(int[] xs) {
        int s = 0;
        for (int i = 0; i < xs.length; i++) {
            int x = xs[i];
            s += (x != 0) ? 1 : 2;
        }
        return s;
    }

    // iload_0; ifeq L1; iload_1; goto L2; L1: iload_2; L2: ifeq ...
    static int ternary(boolean c, int a, int b) {
        if ((c ? a : b) != 0) {
            return 1;
        }
        return 0;
    }

    static long countDownWhile(int n) {
        long s = 0;
        while (n != 0) {
            s += n;
            n--;
        }
        return s;
    }

    static long countDownDo(int n) {
        long s = 0;
        do {
            s += n;
            n--;
        } while (n != 0);
        return s;
    }

    static int flagLoop(int n) {
        boolean running = true;
        int i = 0;
        while (running) {
            i++;
            if (i == n) {
                running = false;
            }
        }
        return i;
    }

    // aload_0; ifnonnull / ifnull
    static int nullTest(Object o) {
        int r = 0;
        if (o == null) {
            r |= 0x1;
        }
        if (o != null) {
            r |= 0x2;
        }
        return r;
    }

    static long walkFor(Node head) {
        long s = 0;
        for (Node n = head; n != null; n = n.next) {
            s += n.v;
        }
        return s;
    }

    static long walkDo(Node head) {
        long s = 0;
        Node n = head;
        do {
            s += n.v;
            n = n.next;
        } while (n != null);
        return s;
    }

    static int countNodes(Node head) {
        int c = 0;
        Node n = head;
        while (n != null) {
            c++;
            n = n.next;
        }
        return c;
    }

    static int nullExpr(Object[] os) {
        int s = 0;
        for (int i = 0; i < os.length; i++) {
            Object o = os[i];
            s += (o == null) ? 1 : 2;
        }
        return s;
    }

    static int nullTernary(boolean c, Object a, Object b) {
        if ((c ? a : b) == null) {
            return 1;
        }
        return 0;
    }

    static String hex(int v) {
        return "0x" + Integer.toHexString(v);
    }

    public static void main(String[] args) {
        int[] values = {Integer.MIN_VALUE, -1, 0, 1, Integer.MAX_VALUE};
        // Warm both shapes so a JIT run also checks the compiled answer.
        int warm = 0;
        for (int k = 0; k < 20_000; k++) {
            warm += zero(values[k % values.length]) + zeroWide(0L, 0L, values[k % values.length]);
        }
        if (warm == 42) {
            System.out.println("unreachable");
        }
        for (int v : values) {
            System.out.println("zero " + v + " " + hex(zero(v)));
        }
        for (int v : values) {
            System.out.println("zeroWide " + v + " " + hex(zeroWide(0L, 0L, v)));
        }

        int[] xs = new int[1003];
        for (int i = 0; i < xs.length; i++) {
            xs[i] = (i % 3 == 0) ? 0 : i;
        }
        // 335 zeros (i = 0, 3, ..., 1002) count 2, the other 668 count 1.
        System.out.println("expr " + expr(xs));

        System.out.println("ternary " + ternary(true, 5, 0) + " " + ternary(true, 0, 5)
                + " " + ternary(false, 0, 5) + " " + ternary(false, 5, 0));

        System.out.println("countDown " + countDownWhile(200_000) + " " + countDownDo(200_000));
        System.out.println("flagLoop " + flagLoop(300_000));

        System.out.println("null null " + hex(nullTest(null)));
        System.out.println("null obj " + hex(nullTest(new Object())));

        Node head = null;
        for (int i = 299_999; i >= 0; i--) {
            Node n = new Node(i);
            n.next = head;
            head = n;
        }
        System.out.println("listWalk " + walkFor(head) + " " + walkDo(head) + " " + countNodes(head));

        Object[] os = new Object[1001];
        for (int i = 0; i < os.length; i++) {
            os[i] = (i % 2 == 0) ? null : Integer.valueOf(i);
        }
        // 501 nulls (even i) count 1, the 500 others count 2.
        System.out.println("nullExpr " + nullExpr(os));

        Object x = new Object();
        System.out.println("nullTernary " + nullTernary(true, null, x) + " " + nullTernary(true, x, null)
                + " " + nullTernary(false, null, x) + " " + nullTernary(false, x, null));
    }
}
