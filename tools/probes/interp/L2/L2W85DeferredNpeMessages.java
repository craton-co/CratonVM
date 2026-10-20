// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 85, lane L2 -- the JEP 358 message of every
// interpreted null-dereference shape, which CRATONVM_INTERP_NPE_DEFERRED_MESSAGE=1
// (the message computed in getMessage() from the stored trace's top frame,
// instead of at the throw) must not move. Also: a message read twice is the
// same String content, a refill before the first read keeps the throw site's
// message (the JDK's fillInStackTrace override computes it first), and an NPE
// whose message is never read costs nothing visible.
//
// Run (stdout must be the block below, byte for byte; compile WITHOUT -g):
//   $J/javac -d out tools/probes/interp/L2/L2W85DeferredNpeMessages.java
//   $B --nojit --java-home $J/.. -cp out L2W85DeferredNpeMessages
//   env CRATONVM_INTERP_NPE_DEFERRED_MESSAGE=1 $B --nojit --java-home $J/.. -cp out L2W85DeferredNpeMessages
//   (both also without --nojit, and with --compatible, where the switch defers nothing)
//   $J/java -Xint -cp out L2W85DeferredNpeMessages
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// getfield Cannot read field "v" because "<parameter1>" is null
// putfield Cannot assign field "v" because "<parameter1>" is null
// chain Cannot read field "v" because "<parameter1>.next" is null
// arraylength Cannot read the array length because "<parameter1>" is null
// iaload Cannot load from int array because "<parameter1>" is null
// aastore Cannot store to object array because "<parameter1>" is null
// monitorenter Cannot enter synchronized block because "<parameter1>" is null
// athrow Cannot throw exception because "<parameter1>" is null
// invokevirtual Cannot invoke "String.length()" because "<parameter1>" is null
// invokeinterface Cannot invoke "java.lang.Runnable.run()" because "<parameter1>" is null
// return-value Cannot invoke "String.length()" because the return value of "L2W85DeferredNpeMessages.nothing()" is null
// static-field Cannot read field "v" because "L2W85DeferredNpeMessages.NODE" is null
// read-twice true
// refilled Cannot read field "v" because "<parameter1>" is null
// HOTSPOT_EXPECTED_END
public class L2W85DeferredNpeMessages {
    static final class Node {
        int v;
        Node next;
    }

    static Node NODE;

    interface Shape {
        void run() throws Throwable;
    }

    static int getfield(Node n) {
        return n.v;
    }

    static void putfield(Node n) {
        n.v = 1;
    }

    static int chain(Node n) {
        return n.next.v;
    }

    static int arraylength(int[] a) {
        return a.length;
    }

    static int iaload(int[] a) {
        return a[0];
    }

    static void aastore(Object[] a) {
        a[0] = "x";
    }

    static void monitorenter(Object o) {
        synchronized (o) {
            o.hashCode();
        }
    }

    static void athrow(RuntimeException e) {
        throw e;
    }

    static int invokevirtual(String s) {
        return s.length();
    }

    static void invokeinterface(Runnable r) {
        r.run();
    }

    static String nothing() {
        return null;
    }

    static int returnValue() {
        return nothing().length();
    }

    static int staticField() {
        return NODE.v;
    }

    static NullPointerException npe(Shape s) {
        try {
            s.run();
        } catch (NullPointerException e) {
            return e;
        } catch (Throwable t) {
            throw new AssertionError(t);
        }
        throw new AssertionError("no NPE");
    }

    static void row(String label, Shape s) {
        System.out.println(label + " " + npe(s).getMessage());
    }

    public static void main(String[] args) {
        row("getfield", () -> getfield(null));
        row("putfield", () -> putfield(null));
        row("chain", () -> chain(new Node()));
        row("arraylength", () -> arraylength(null));
        row("iaload", () -> iaload(null));
        row("aastore", () -> aastore(null));
        row("monitorenter", () -> monitorenter(null));
        row("athrow", () -> athrow(null));
        row("invokevirtual", () -> invokevirtual(null));
        row("invokeinterface", () -> invokeinterface(null));
        row("return-value", L2W85DeferredNpeMessages::returnValue);
        row("static-field", L2W85DeferredNpeMessages::staticField);

        NullPointerException twice = npe(() -> getfield(null));
        System.out.println("read-twice " + twice.getMessage().equals(twice.getMessage()));

        NullPointerException refilled = npe(() -> getfield(null));
        refill(refilled);
        System.out.println("refilled " + refilled.getMessage());

        // Thrown and dropped, never read: nothing to print.
        for (int i = 0; i < 1000; i++) {
            npe(() -> getfield(null));
        }
    }

    static void refill(Throwable t) {
        t.fillInStackTrace();
    }
}
