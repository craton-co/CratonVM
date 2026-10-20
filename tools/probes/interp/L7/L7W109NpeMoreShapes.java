// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 109, lane L7: JEP 358 helpful NPE texts for the less common shapes
// wave 106's L7W106HelpfulNpeShapes did not cover: a receiver that comes through a conditional
// (two producers -> HotSpot drops the "because" clause), a null literal cast, checkcast
// transparency for generic returns (Supplier.get / List.get), `this`-rooted chains at and past
// the five-part detail cap, Long/Boolean unboxing, switch on a null String / enum, String and
// StringBuilder receivers with String parameters (trim_well_known_class_names), array parameter
// spellings, compound assignment and `a[0]++` (dup / dup2 producers), a method-reference receiver
// (javac's Objects.requireNonNull: an explicitly constructed NPE, getMessage() == null),
// for-each over a null List, a captured null inside a lambda body, synchronized / throw of a
// null field, and long / char / boolean array element kinds.
//
// Compile WITHOUT -g (javac's default: no LocalVariableTable), so locals print as
// <localN> / <parameterN>:
//   javac -d out L7W109NpeMoreShapes.java
//   java -cp out L7W109NpeMoreShapes
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L7W109NpeMoreShapes
// Each row runs once, cold. The local numbers in string-switch depend on javac's desugaring and
// the HotSpot run is the authority for them.
// HOTSPOT_EXPECTED_BEGIN (JDK 25; predicted from bytecodeUtils.cpp)
// ternary -> Cannot read field "value"
// null-literal -> Cannot invoke "String.length()" because "null" is null
// checkcast-param -> Cannot read field "value" because "<parameter1>" is null
// supplier-get -> Cannot invoke "String.length()" because the return value of "java.util.function.Supplier.get()" is null
// list-get -> Cannot read field "value" because the return value of "java.util.List.get(int)" is null
// this-chain -> Cannot read field "value" because "this.a.b.c.d" is null
// this-chain-capped -> Cannot read field "value" because "a.b.c.d.e" is null
// unbox-long -> Cannot invoke "java.lang.Long.longValue()" because "<parameter1>" is null
// unbox-boolean -> Cannot invoke "java.lang.Boolean.booleanValue()" because "<parameter1>" is null
// string-switch -> Cannot invoke "String.hashCode()" because "<local1>" is null
// enum-switch -> Cannot invoke "L7W109NpeMoreShapes$Color.ordinal()" because "<parameter1>" is null
// string-concat-call -> Cannot invoke "String.concat(String)" because "<parameter1>" is null
// builder-append -> Cannot invoke "java.lang.StringBuilder.append(String)" because "<parameter1>" is null
// array-params -> Cannot invoke "L7W109NpeMoreShapes$Node.mix(int[], String[], java.util.List)" because "<parameter1>" is null
// compound-assign -> Cannot read field "value" because "<parameter1>" is null
// element-increment -> Cannot load from int array because "<parameter1>" is null
// method-ref -> null
// foreach-list -> Cannot invoke "java.util.List.iterator()" because "<parameter1>" is null
// lambda-capture -> Cannot read field "value" because "<parameter1>" is null
// sync-field -> Cannot enter synchronized block because "this.lock" is null
// throw-field -> Cannot throw exception because "this.err" is null
// putfield-chain -> Cannot assign field "value" because "<parameter1>.a" is null
// long-2d -> Cannot load from long array because "<parameter1>[<parameter2>]" is null
// char-store -> Cannot store to char array because "<parameter1>" is null
// boolean-load -> Cannot load from byte/boolean array because "L7W109NpeMoreShapes.FLAGS" is null
// static-call-index -> Cannot read field "value" because "L7W109NpeMoreShapes.arr()[0]" is null
// HOTSPOT_EXPECTED_END
import java.util.ArrayList;
import java.util.List;
import java.util.function.IntSupplier;
import java.util.function.Supplier;

public class L7W109NpeMoreShapes {
    enum Color { RED, GREEN }

    static final class Node {
        int value;

        int mix(int[] a, String[] b, List<?> c) {
            return value;
        }

        int touch() {
            return value;
        }
    }

    static final class Holder {
        Holder a;
        Holder b;
        Holder c;
        Holder d;
        Holder e;
        int value;
        Object lock;
        RuntimeException err;

        int chain() {
            return this.a.b.c.d.value;
        }

        int chainCapped() {
            return this.a.b.c.d.e.value;
        }

        void sync() {
            synchronized (this.lock) {
                value++;
            }
        }

        void doThrow() {
            throw this.err;
        }
    }

    static boolean[] FLAGS;
    static int sink;

    static int ternary(boolean f, Node x, Node y) {
        return (f ? x : y).value;
    }

    static int nullLiteral() {
        return ((String) null).length();
    }

    static int checkcastParam(Object o) {
        return ((Node) o).value;
    }

    static int supplierGet(Supplier<String> s) {
        return s.get().length();
    }

    static int listGet(List<Node> l) {
        return l.get(0).value;
    }

    static long unboxLong(Long v) {
        return v + 1;
    }

    static int unboxBoolean(Boolean b) {
        if (b) {
            return 1;
        }
        return 0;
    }

    static int stringSwitch(String s) {
        switch (s) {
            case "a":
                return 1;
            case "b":
                return 2;
            default:
                return 0;
        }
    }

    static int enumSwitch(Color c) {
        switch (c) {
            case RED:
                return 1;
            case GREEN:
                return 2;
            default:
                return 0;
        }
    }

    static String concatCall(String s) {
        return s.concat("x");
    }

    static StringBuilder builderAppend(StringBuilder sb) {
        return sb.append("x");
    }

    static int arrayParams(Node n) {
        return n.mix(null, null, null);
    }

    static void compoundAssign(Node n) {
        n.value += 1;
    }

    static void elementIncrement(int[] a) {
        a[0]++;
    }

    static IntSupplier methodRef(Node n) {
        return n::touch;
    }

    static int foreachList(List<Node> l) {
        int s = 0;
        for (Node n : l) {
            s += n.value;
        }
        return s;
    }

    static IntSupplier lambdaCapture(Node n) {
        return () -> n.value;
    }

    static void putfieldChain(Holder h) {
        h.a.value = 3;
    }

    static long long2d(long[][] g, int i) {
        return g[i][0];
    }

    static void charStore(char[] c) {
        c[0] = 'x';
    }

    static boolean booleanLoad() {
        return FLAGS[0];
    }

    static Node[] arr() {
        return new Node[1];
    }

    static int staticCallIndex() {
        return arr()[0].value;
    }

    interface Row {
        Object run() throws Throwable;
    }

    static void row(String name, Row r) {
        String answer;
        try {
            answer = "no exception: " + r.run();
        } catch (NullPointerException e) {
            answer = String.valueOf(e.getMessage());
        } catch (Throwable t) {
            answer = t.toString();
        }
        System.out.println(name + " -> " + answer);
    }

    public static void main(String[] args) {
        Node live = new Node();
        row("ternary", () -> ternary(true, null, live));
        row("null-literal", L7W109NpeMoreShapes::nullLiteral);
        row("checkcast-param", () -> checkcastParam(null));
        row("supplier-get", () -> supplierGet(() -> null));
        List<Node> withNull = new ArrayList<>();
        withNull.add(null);
        row("list-get", () -> listGet(withNull));
        Holder h = new Holder();
        h.a = new Holder();
        h.a.b = new Holder();
        h.a.b.c = new Holder();
        row("this-chain", h::chain);
        h.a.b.c.d = new Holder();
        row("this-chain-capped", h::chainCapped);
        row("unbox-long", () -> unboxLong(null));
        row("unbox-boolean", () -> unboxBoolean(null));
        row("string-switch", () -> stringSwitch(null));
        row("enum-switch", () -> enumSwitch(null));
        row("string-concat-call", () -> concatCall(null));
        row("builder-append", () -> builderAppend(null));
        row("array-params", () -> arrayParams(null));
        row("compound-assign", () -> {
            compoundAssign(null);
            return "stored";
        });
        row("element-increment", () -> {
            elementIncrement(null);
            return "stored";
        });
        row("method-ref", () -> methodRef(null));
        row("foreach-list", () -> foreachList(null));
        row("lambda-capture", () -> lambdaCapture(null).getAsInt());
        row("sync-field", () -> {
            new Holder().sync();
            return "entered";
        });
        row("throw-field", () -> {
            new Holder().doThrow();
            return "returned";
        });
        row("putfield-chain", () -> {
            putfieldChain(new Holder());
            return "stored";
        });
        row("long-2d", () -> long2d(new long[2][], 1));
        row("char-store", () -> {
            charStore(null);
            return "stored";
        });
        row("boolean-load", L7W109NpeMoreShapes::booleanLoad);
        row("static-call-index", L7W109NpeMoreShapes::staticCallIndex);
    }
}
