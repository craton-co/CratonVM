// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 99, lane L9 -- an implicit NullPointerException's
// JEP 358 text lives in the TRANSIENT extendedMessage, never in detailMessage
// (extends L9W98NpeMessageSerialized). HotSpot builds the NPE with no message;
// getMessage() computes the text into extendedMessage (state 2), so every
// reader of the message (getLocalizedMessage, toString, printStackTrace, a
// wrapping RuntimeException(cause)) sees it, while a serialized copy answers
// null. An NPE built with a message, or by an application subclass, keeps a
// real detailMessage that survives the round trip.
//
// CratonVM --compatible stored the text in detailMessage, so the *-roundtrip
// rows kept it. See
// docs/internal/fixed-bugs/interpreter-i98-L9-compatible-implicit-npe-text-is-a-detail-message-FIXED-20261015.md.
// The `compiled*` rows throw 20000 times so a JIT-compiled body raises the
// last one (in --nojit the interpreter raises it: same answer).
//
// No -g: the null operand is named "<parameter1>".
//
//   $J/javac -d out tools/probes/interp/L9/L9W99NpeMessageSerialized.java
//   $B [--compatible] [--nojit] --java-home $J/.. -cp out L9W99NpeMessageSerialized
//   $J/java -cp out L9W99NpeMessageSerialized
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25)
// implicit: Cannot read field "x" because "<parameter1>" is null
// implicit-localized: Cannot read field "x" because "<parameter1>" is null
// implicit-toString: java.lang.NullPointerException: Cannot read field "x" because "<parameter1>" is null
// implicit-printStackTrace: java.lang.NullPointerException: Cannot read field "x" because "<parameter1>" is null
// implicit-roundtrip: null
// implicit-roundtrip-toString: java.lang.NullPointerException
// implicit-unread-roundtrip: null
// invoke: Cannot invoke "String.length()" because "<parameter1>" is null
// invoke-unread-roundtrip: null
// refilled: Cannot read field "x" because "<parameter1>" is null
// refilled-roundtrip: null
// wrapped: java.lang.NullPointerException: Cannot read field "x" because "<parameter1>" is null
// wrapped-roundtrip: java.lang.NullPointerException: Cannot read field "x" because "<parameter1>" is null
// wrapped-roundtrip-cause: null
// compiled: Cannot read field "x" because "<parameter1>" is null
// compiled-roundtrip: null
// constructed-roundtrip: given
// constructed-empty: null
// constructed-empty-roundtrip: null
// subclass: null
// subclass-roundtrip: null
// subclass-given-roundtrip: sub
// HOTSPOT_EXPECTED_END
import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.ObjectInputStream;
import java.io.ObjectOutputStream;
import java.io.PrintWriter;
import java.io.StringWriter;

public class L9W99NpeMessageSerialized {
    static final class P {
        int x;
    }

    static final class MyNpe extends NullPointerException {
        MyNpe() {
        }

        MyNpe(String s) {
            super(s);
        }
    }

    static int read(P p) {
        return p.x;
    }

    static int len(String s) {
        return s.length();
    }

    static NullPointerException implicitNpe() {
        try {
            read(null);
        } catch (NullPointerException e) {
            return e;
        }
        throw new AssertionError();
    }

    static NullPointerException invokeNpe() {
        try {
            len(null);
        } catch (NullPointerException e) {
            return e;
        }
        throw new AssertionError();
    }

    static NullPointerException compiledNpe() {
        NullPointerException last = null;
        for (int i = 0; i < 20000; i++) {
            try {
                read(null);
            } catch (NullPointerException e) {
                last = e;
            }
        }
        return last;
    }

    static Throwable roundTrip(Throwable t) throws Exception {
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        try (ObjectOutputStream out = new ObjectOutputStream(bytes)) {
            out.writeObject(t);
        }
        try (ObjectInputStream in = new ObjectInputStream(new ByteArrayInputStream(bytes.toByteArray()))) {
            return (Throwable) in.readObject();
        }
    }

    static String firstTraceLine(Throwable t) {
        StringWriter sw = new StringWriter();
        try (PrintWriter pw = new PrintWriter(sw)) {
            t.printStackTrace(pw);
        }
        return sw.toString().split("\\R", 2)[0];
    }

    public static void main(String[] args) throws Exception {
        NullPointerException read = implicitNpe();
        System.out.println("implicit: " + read.getMessage());
        System.out.println("implicit-localized: " + read.getLocalizedMessage());
        System.out.println("implicit-toString: " + read);
        System.out.println("implicit-printStackTrace: " + firstTraceLine(read));
        Throwable back = roundTrip(read);
        System.out.println("implicit-roundtrip: " + back.getMessage());
        System.out.println("implicit-roundtrip-toString: " + back);
        System.out.println("implicit-unread-roundtrip: " + roundTrip(implicitNpe()).getMessage());

        NullPointerException invoke = invokeNpe();
        System.out.println("invoke: " + invoke.getMessage());
        System.out.println("invoke-unread-roundtrip: " + roundTrip(invokeNpe()).getMessage());

        // fillInStackTrace() before any getMessage(): the JDK computes the text
        // from the original trace first; setStackTrace does not change it.
        NullPointerException refilled = implicitNpe();
        refilled.fillInStackTrace();
        refilled.setStackTrace(new StackTraceElement[0]);
        System.out.println("refilled: " + refilled.getMessage());
        System.out.println("refilled-roundtrip: " + roundTrip(refilled).getMessage());

        RuntimeException wrapped = new RuntimeException(implicitNpe());
        System.out.println("wrapped: " + wrapped.getMessage());
        Throwable wrappedBack = roundTrip(wrapped);
        System.out.println("wrapped-roundtrip: " + wrappedBack.getMessage());
        System.out.println("wrapped-roundtrip-cause: " + wrappedBack.getCause().getMessage());

        NullPointerException compiled = compiledNpe();
        System.out.println("compiled: " + compiled.getMessage());
        System.out.println("compiled-roundtrip: " + roundTrip(compiledNpe()).getMessage());

        System.out.println("constructed-roundtrip: " + roundTrip(new NullPointerException("given")).getMessage());
        NullPointerException empty = new NullPointerException();
        System.out.println("constructed-empty: " + empty.getMessage());
        System.out.println("constructed-empty-roundtrip: " + roundTrip(new NullPointerException()).getMessage());
        System.out.println("subclass: " + new MyNpe().getMessage());
        System.out.println("subclass-roundtrip: " + roundTrip(new MyNpe()).getMessage());
        System.out.println("subclass-given-roundtrip: " + roundTrip(new MyNpe("sub")).getMessage());
    }
}
