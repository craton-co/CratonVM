// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 83, lane L7: what the `aload n; getfield`
// fusion (CRATONVM_INTERP_FUSED_WIDE_LOCAL_GETFIELD, default off;
// interpreter-i72-L7-proposal-wide-local-forms-of-the-fused-groups-KEPT-OFF-20261019) must leave
// unchanged: every field type read through a receiver past local 3 (the two
// leading `long` parameters put it in local 4), a subclass instance through
// its superclass's field, a linked-list walk whose cursor lives in local 7,
// and a null receiver whose NPE the `getfield` raises at its own pc after
// warm hits. Every method runs several times so the field sites are warm.
//
// Run with the switch off and on; both must print the block below, with and
// without --nojit:
//   cratonvm --java-home <jdk25> --nojit -cp <dir> L7W83WideLocalGetfield
//   CRATONVM_INTERP_FUSED_WIDE_LOCAL_GETFIELD=1 cratonvm --java-home <jdk25> --nojit -cp <dir> L7W83WideLocalGetfield
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// read 7 9000000000 2.5 -3 65 str false
// walk 5050
// sub 11 12
// npe java.lang.NullPointerException warm=5
// HOTSPOT_EXPECTED_END
public class L7W83WideLocalGetfield {
    static class Node {
        int i;
        long l;
        double d;
        byte b;
        char c;
        Object o;
        Node next;
    }

    static final class Sub extends Node {
        int j;
    }

    // n: local 4 (`aload 4; getfield` for every read).
    static String read(long pad0, long pad1, Node n) {
        int i = n.i;
        long l = n.l;
        double d = n.d;
        byte b = n.b;
        char c = n.c;
        Object o = n.o;
        boolean last = n.next == null;
        return i + " " + l + " " + d + " " + b + " " + (int) c + " " + o + " " + last;
    }

    // head: local 4; s: 5..6; n: 7 (`aload 7; getfield i`, `aload 7;
    // getfield next`).
    static long walk(long pad0, long pad1, Node head) {
        long s = 0;
        for (Node n = head; n != null; n = n.next) {
            s += n.i;
        }
        return s;
    }

    // s: local 4, a `Sub` read through `Node.i` and its own `j`.
    static String sub(long pad0, long pad1, Sub s) {
        return s.i + " " + s.j;
    }

    // n: local 4.
    static int readInt(long pad0, long pad1, Node n) {
        return n.i;
    }

    public static void main(String[] args) {
        Node n = new Node();
        n.i = 7;
        n.l = 9_000_000_000L;
        n.d = 2.5;
        n.b = -3;
        n.c = 'A';
        n.o = "str";
        n.next = new Node();
        String r = "";
        for (int k = 0; k < 5; k++) {
            r = read(0, 0, n);
        }
        System.out.println("read " + r);

        Node head = null;
        for (int v = 100; v >= 1; v--) {
            Node x = new Node();
            x.i = v;
            x.next = head;
            head = x;
        }
        long w = 0;
        for (int k = 0; k < 5; k++) {
            w = walk(0, 0, head);
        }
        System.out.println("walk " + w);

        Sub s = new Sub();
        s.i = 11;
        s.j = 12;
        String t = "";
        for (int k = 0; k < 5; k++) {
            t = sub(0, 0, s);
        }
        System.out.println("sub " + t);

        int warm = 0;
        try {
            for (int k = 0; k < 6; k++) {
                readInt(0, 0, k < 5 ? n : null);
                warm++;
            }
            System.out.println("npe none warm=" + warm);
        } catch (NullPointerException e) {
            System.out.println("npe " + e.getClass().getName() + " warm=" + warm);
        }
    }
}
