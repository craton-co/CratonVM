// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.*;
import java.lang.reflect.Constructor;
import java.util.*;
import sun.reflect.ReflectionFactory;

/**
 * The JDK 21 serialization round-trip defect, at both levels it is visible at.
 *
 * See docs/known-issues/jdk-only/
 *     jdk-21-serialization-round-trip-returns-the-wrong-class-20260909.md
 *
 * Part A drives ObjectInputStream, which is what the corpus row does. Part B
 * drives the factory underneath it, which is where the contract is actually
 * broken -- run both, because A's `ArrayList` row throws and would otherwise
 * hide the SILENT rows behind it, which is exactly how this was missed once.
 *
 * The oracle for every row is the same command on HotSpot.
 *
 *   java   --add-opens=java.base/java.util=ALL-UNNAMED -cp probes Jdk21SerializationCarrier
 *   cratonvm --real-jdk --add-opens=java.base/java.util=ALL-UNNAMED -cp probes Jdk21SerializationCarrier
 *
 * `--add-opens` is needed by part B only, and only for the abstract-ancestor
 * rows: `setAccessible` on `AbstractList()` is refused without it. That is a
 * property of this probe, not of the defect.
 */
public class Jdk21SerializationCarrier {

    static byte[] enc(Object o) throws Exception {
        ByteArrayOutputStream bo = new ByteArrayOutputStream();
        try (ObjectOutputStream oo = new ObjectOutputStream(bo)) { oo.writeObject(o); }
        return bo.toByteArray();
    }

    static Object dec(byte[] b) throws Exception {
        try (ObjectInputStream oi = new ObjectInputStream(new ByteArrayInputStream(b))) {
            return oi.readObject();
        }
    }

    /** A: the round trip. Reports the CLASS, not just the value -- the silent
     *  half of this defect round-trips a plausible-looking object. */
    static void roundTrip(String label, Object o) {
        try {
            Object back = dec(enc(o));
            String read = back == null ? "null" : back.getClass().getName();
            boolean ok = back != null && back.getClass() == o.getClass();
            System.out.println("A " + (ok ? "OK   " : "WRONG") + " " + label
                    + " wrote=" + o.getClass().getName() + " read=" + read + " value=" + back);
        } catch (Throwable t) {
            System.out.println("A FAIL  " + label + " -> " + t);
        }
    }

    /** B: the factory. `newConstructorForSerialization` must return a
     *  constructor DECLARED BY the first non-serializable ancestor that
     *  ALLOCATES the target -- those two classes differ, and that is the
     *  whole contract. */
    static void factory(Class<?> target) {
        try {
            Constructor<?> c = ReflectionFactory.getReflectionFactory()
                    .newConstructorForSerialization(target);
            if (c == null) { System.out.println("B NULL  " + target.getName()); return; }
            c.setAccessible(true);
            String made;
            try {
                Object o = c.newInstance();
                made = o == null ? "null" : o.getClass().getName();
            } catch (Throwable t) {
                made = "THREW " + t.getClass().getName();
            }
            boolean ok = made.equals(target.getName());
            System.out.println("B " + (ok ? "OK   " : "WRONG") + " target=" + target.getName()
                    + " declaredBy=" + c.getDeclaringClass().getName() + " allocates=" + made);
        } catch (Throwable t) {
            System.out.println("B ERR   " + target.getName() + " -> " + t);
        }
    }

    public static void main(String[] args) {
        // Concrete first non-serializable ancestor (Object) -> the SILENT half.
        roundTrip("Integer", 42);
        roundTrip("Long", 7L);
        roundTrip("Boolean", Boolean.TRUE);
        // Abstract first non-serializable ancestor -> the LOUD half.
        roundTrip("ArrayList", new ArrayList<>(List.of("a", "b")));
        roundTrip("HashMap", new HashMap<>(Map.of("k", "v")));
        // Control: handled by the stream itself (TC_STRING), never by the
        // constructor path, so it must pass even when everything else fails.
        roundTrip("String", "hello");

        factory(Integer.class);
        factory(Long.class);
        factory(ArrayList.class);
        factory(HashMap.class);
    }
}
