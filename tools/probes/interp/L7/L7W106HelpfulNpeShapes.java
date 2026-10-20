// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 106, lane L7 (hunt): JEP 358 helpful NPE texts for the shapes the
// wave brief lists -- static field chains, arrays of arrays (local and static), an index that is
// itself an array element, invoke results inline and at top level, unboxing, for-each over a
// null array (javac's hidden copy local), synchronized / throw of a null parameter, a static
// array's length, a field-array-field chain, and the five-level detail cap
// (`ExceptionMessageBuilder::_max_cause_detail`). Each row throws once cold, then once
// more after the methods are warm (20000 non-throwing calls each) to cover the JIT arm.
//
// Warm rows print the exception class only (HotSpot's -XX:+OmitStackTraceInFastThrow may drop
// the message and frames once C2 compiles a site); the cold rows carry the texts.
//
// Compile WITHOUT -g (javac's default: no LocalVariableTable), so locals print as
// <localN> / <parameterN>:
//   javac -d out L7W106HelpfulNpeShapes.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L7W106HelpfulNpeShapes
// HOTSPOT_EXPECTED_BEGIN
// cold static-chain -> Cannot read field "value" because "L7W106HelpfulNpeShapes.ROOT.next" is null
// cold local-2d -> Cannot load from int array because "<parameter1>[1]" is null
// cold static-2d -> Cannot store to int array because "L7W106HelpfulNpeShapes.GRID[2]" is null
// cold index-of-index -> Cannot read field "value" because "<parameter1>[<parameter2>[0]]" is null
// cold invoke-inline -> Cannot read field "value" because "L7W106HelpfulNpeShapes.make().next" is null
// cold invoke-top -> Cannot read field "value" because the return value of "L7W106HelpfulNpeShapes.nothing()" is null
// cold unbox -> Cannot invoke "java.lang.Integer.intValue()" because "<parameter1>" is null
// cold foreach -> Cannot read the array length because "<local2>" is null
// cold sync -> Cannot enter synchronized block because "<parameter1>" is null
// cold throw -> Cannot throw exception because "<parameter1>" is null
// cold static-length -> Cannot read the array length because "L7W106HelpfulNpeShapes.NOARR" is null
// cold field-array-field -> Cannot read field "data" because "<parameter1>.kids[0]" is null
// cold depth-cap -> Cannot read field "value" because "next.next.next.next.next" is null
// cold long-arg-receiver -> Cannot invoke "L7W106HelpfulNpeShapes$Node.take(long, double, int)" because "<parameter1>" is null
// cold string-receiver -> Cannot invoke "String.length()" because "<parameter2>" is null
// warm static-chain -> java.lang.NullPointerException
// warm local-2d -> java.lang.NullPointerException
// warm static-2d -> java.lang.NullPointerException
// warm index-of-index -> java.lang.NullPointerException
// warm invoke-inline -> java.lang.NullPointerException
// warm invoke-top -> java.lang.NullPointerException
// warm unbox -> java.lang.NullPointerException
// warm foreach -> java.lang.NullPointerException
// warm sync -> java.lang.NullPointerException
// warm throw -> java.lang.NullPointerException
// warm static-length -> java.lang.NullPointerException
// warm field-array-field -> java.lang.NullPointerException
// warm depth-cap -> java.lang.NullPointerException
// warm long-arg-receiver -> java.lang.NullPointerException
// warm string-receiver -> java.lang.NullPointerException
// HOTSPOT_EXPECTED_END
public class L7W106HelpfulNpeShapes {
    static final class Node {
        Node next;
        int value;
        int[] data;
        Node[] kids;

        int take(long a, double b, int c) {
            return (int) a + (int) b + c;
        }
    }

    static Node ROOT = new Node();
    static int[][] GRID = new int[3][];
    static int[] NOARR;
    static int sink;

    static Node make() {
        return new Node();
    }

    static Node nothing() {
        return null;
    }

    static int staticChain() {
        return ROOT.next.value;
    }

    static int local2d(int[][] m) {
        return m[1][0];
    }

    static void static2d() {
        GRID[2][1] = 7;
    }

    static int indexOfIndex(Node[] ns, int[] idx) {
        return ns[idx[0]].value;
    }

    static int invokeInline() {
        return make().next.value;
    }

    static int invokeTop() {
        return nothing().value;
    }

    static int unbox(Integer i) {
        return i;
    }

    static int foreach(int[] xs) {
        int s = 0;
        for (int x : xs) {
            s += x;
        }
        return s;
    }

    static void sync(Object o) {
        synchronized (o) {
            sink++;
        }
    }

    static void doThrow(RuntimeException e) {
        throw e;
    }

    static int staticLength() {
        return NOARR.length;
    }

    static void fieldArrayField(Node n) {
        n.kids[0].data[1] = 3;
    }

    static int depthCap(Node n) {
        return n.next.next.next.next.next.next.value;
    }

    static int longArgReceiver(Node n, long a, double b) {
        return n.take(a, b, 3);
    }

    static int stringReceiver(int pad, String s) {
        return s.length() + pad;
    }

    // Warm rows print the exception CLASS only: once C2 has compiled a site, HotSpot may throw a
    // preallocated stackless exception without a message (-XX:+OmitStackTraceInFastThrow), so the
    // text and frames there depend on JIT timing. The texts are checked by the cold rows.
    static final String WARM = "warm";

    interface Row {
        Object run() throws Throwable;
    }

    static void row(String phase, String name, Row r) {
        String answer;
        try {
            answer = "no exception: " + r.run();
        } catch (NullPointerException e) {
            answer = WARM.equals(phase) ? e.getClass().getName() : e.getMessage();
        } catch (Throwable t) {
            answer = WARM.equals(phase) ? t.getClass().getName() : t.toString();
        }
        System.out.println(phase + " " + name + " -> " + answer);
    }

    static Node chain(int length) {
        Node head = new Node();
        Node cur = head;
        for (int i = 0; i < length; i++) {
            cur.next = new Node();
            cur = cur.next;
        }
        return head;
    }

    static void rows(String phase) {
        row(phase, "static-chain", L7W106HelpfulNpeShapes::staticChain);
        row(phase, "local-2d", () -> local2d(new int[2][]));
        row(phase, "static-2d", () -> {
            static2d();
            return "stored";
        });
        row(phase, "index-of-index", () -> indexOfIndex(new Node[2], new int[] {1}));
        row(phase, "invoke-inline", L7W106HelpfulNpeShapes::invokeInline);
        row(phase, "invoke-top", L7W106HelpfulNpeShapes::invokeTop);
        row(phase, "unbox", () -> unbox(null));
        row(phase, "foreach", () -> foreach(null));
        row(phase, "sync", () -> {
            sync(null);
            return "entered";
        });
        row(phase, "throw", () -> {
            doThrow(null);
            return "returned";
        });
        row(phase, "static-length", L7W106HelpfulNpeShapes::staticLength);
        Node withKids = new Node();
        withKids.kids = new Node[1];
        row(phase, "field-array-field", () -> {
            fieldArrayField(withKids);
            return "stored";
        });
        row(phase, "depth-cap", () -> depthCap(chain(5)));
        row(phase, "long-arg-receiver", () -> longArgReceiver(null, 1L, 2.0));
        row(phase, "string-receiver", () -> stringReceiver(1, null));
    }

    static void warm() {
        int[][] m = {{1}, {2}};
        int[][] grid = GRID;
        Node[] ns = {new Node(), new Node()};
        int[] idx = {1};
        Node ok = make();
        ok.next = make();
        Node kids = new Node();
        kids.kids = new Node[] {new Node()};
        kids.kids[0].data = new int[2];
        Node deep = chain(6);
        ROOT.next = new Node();
        GRID = new int[3][2];
        NOARR = new int[1];
        for (int i = 0; i < 20_000; i++) {
            sink += staticChain() + local2d(m) + indexOfIndex(ns, idx) + unbox(i) + foreach(idx)
                + staticLength() + depthCap(deep) + longArgReceiver(ok, 1L, 2.0) + stringReceiver(1, "ab");
            static2d();
            sync(ok);
            fieldArrayField(kids);
            sink += make().value;
            try {
                doThrow(new IllegalStateException());
            } catch (IllegalStateException expected) {
                sink++;
            }
        }
        ROOT.next = null;
        GRID = grid;
        NOARR = null;
    }

    public static void main(String[] args) {
        rows("cold");
        warm();
        rows("warm");
    }
}
