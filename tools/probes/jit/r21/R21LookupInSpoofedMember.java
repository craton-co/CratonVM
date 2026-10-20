// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 13, review lane rev-native (page
// docs/internal/fixed-bugs/r21-review-w13-native-findings-FIXED-20261019.md item 3):
// Lookup.in, getDeclaringClass and getEnclosingClass for NON-hidden classes
// whose InnerClasses attributes disagree.
//
// HotSpot's compute_enclosing_class (JVM_GetDeclaringClass) resolves the outer
// class named by a class's own InnerClasses entry. It can load that class and
// throw NoClassDefFoundError. It then runs Reflection::check_for_inner_class,
// which throws IncompatibleClassChangeError "<outer> and <inner> disagree on
// InnerClasses attribute" unless the outer class's InnerClasses confirms the
// pair. JDK 25's Lookup.in reaches that check through
// VerifyAccess.isSamePackageMember -> getOutermostEnclosingClass ->
// getEnclosingClass, but only while PRIVATE survives (same module, same
// package). This VM's Lookup.in walk (classloader::lk_outermost_enclosing)
// refuses only HIDDEN member links today. This probe measures what HotSpot does
// for ordinary classes.
//
// Every class is built here as raw class-file bytes (no methods, no fields) and
// defined by one custom loader, all in package r21spoof:
//   O1 / G1     G1 claims "member of O1"; O1 has no InnerClasses   (spoof)
//   O2 / M2     M2 claims O2, O2 lists (M2, O2)                    (consistent control)
//   G3          G3 claims member of r21spoof.Missing (no such class)
//   O4 / G4     G4 claims O4; O4 lists (G4, outer r21spoof.Other)  (outer disagrees)
//   O5 / T5     O5 lists (T5, O5); T5 has no InnerClasses           (one-sided, top-level T5)
// Lookups are MethodHandles.privateLookupIn(X, lookup()): another module than the
// probe's, so no MODULE bit, but PRIVATE and PACKAGE are kept, and in() between
// two classes of the custom loader's unnamed module skips the module step.
// Each row prints the result (lookupModes, or a class name) or the exception's
// class and message, plus its cause's class when it has one.
//
// Run:
//   javac -d out R21LookupInSpoofedMember.java
//   java -cp out R21LookupInSpoofedMember                         # HotSpot 25
//   cratonvm -cp out R21LookupInSpoofedMember                     # --jdk-only
//   cratonvm --compatible -cp out R21LookupInSpoofedMember
// Or as a single source file: java R21LookupInSpoofedMember.java
// Expectation: not recorded yet. Diff this VM's output against HotSpot's.

import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.lang.invoke.MethodHandles;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.concurrent.Callable;

public class R21LookupInSpoofedMember {
    static final String PKG = "r21spoof/";

    /** One InnerClasses row: inner, outer (null = 0), simple name (null = 0). */
    record Inner(String inner, String outer, String simple) {}

    /** A minimal class file: public super class, Object superclass, optional InnerClasses. */
    static byte[] classBytes(String name, List<Inner> inners) {
        try {
            List<byte[]> pool = new ArrayList<>();
            Map<String, Integer> utf8 = new HashMap<>();
            Map<String, Integer> classes = new HashMap<>();
            java.util.function.Function<String, Integer> u = s -> utf8.computeIfAbsent(s, k -> {
                try {
                    ByteArrayOutputStream b = new ByteArrayOutputStream();
                    DataOutputStream d = new DataOutputStream(b);
                    d.writeByte(1);
                    d.writeUTF(k);
                    pool.add(b.toByteArray());
                    return pool.size();
                } catch (IOException e) {
                    throw new RuntimeException(e);
                }
            });
            java.util.function.Function<String, Integer> c = s -> {
                Integer have = classes.get(s);
                if (have != null) return have;
                int nameIndex = u.apply(s);
                pool.add(new byte[] {7, (byte) (nameIndex >> 8), (byte) nameIndex});
                classes.put(s, pool.size());
                return pool.size();
            };
            int thisIndex = c.apply(name);
            int superIndex = c.apply("java/lang/Object");
            int attrName = inners.isEmpty() ? 0 : u.apply("InnerClasses");
            int[][] rows = new int[inners.size()][];
            for (int i = 0; i < inners.size(); i++) {
                Inner in = inners.get(i);
                rows[i] = new int[] {
                    c.apply(in.inner()),
                    in.outer() == null ? 0 : c.apply(in.outer()),
                    in.simple() == null ? 0 : u.apply(in.simple()),
                    0x0009, // public static
                };
            }
            ByteArrayOutputStream out = new ByteArrayOutputStream();
            DataOutputStream d = new DataOutputStream(out);
            d.writeInt(0xCAFEBABE);
            d.writeShort(0);
            d.writeShort(61); // Java 17 class files: no StackMapTable needed (no code at all)
            d.writeShort(pool.size() + 1);
            for (byte[] e : pool) d.write(e);
            d.writeShort(0x0021); // ACC_PUBLIC | ACC_SUPER
            d.writeShort(thisIndex);
            d.writeShort(superIndex);
            d.writeShort(0); // interfaces
            d.writeShort(0); // fields
            d.writeShort(0); // methods
            if (inners.isEmpty()) {
                d.writeShort(0);
            } else {
                d.writeShort(1);
                d.writeShort(attrName);
                d.writeInt(2 + 8 * rows.length);
                d.writeShort(rows.length);
                for (int[] r : rows) {
                    for (int v : r) d.writeShort(v);
                }
            }
            d.flush();
            return out.toByteArray();
        } catch (IOException e) {
            throw new RuntimeException(e);
        }
    }

    static final class Loader extends ClassLoader {
        final Map<String, byte[]> bytes = new HashMap<>();

        Loader() {
            super("r21spoof", R21LookupInSpoofedMember.class.getClassLoader());
        }

        void add(String simple, List<Inner> inners) {
            String internal = PKG + simple;
            bytes.put(internal.replace('/', '.'), classBytes(internal, inners));
        }

        @Override
        protected Class<?> findClass(String name) throws ClassNotFoundException {
            byte[] b = bytes.get(name);
            if (b == null) throw new ClassNotFoundException(name);
            return defineClass(name, b, 0, b.length);
        }
    }

    static void row(String label, Callable<Object> body) {
        String text;
        try {
            Object v = body.call();
            text = v instanceof Class<?> k ? "class " + k.getName() : String.valueOf(v);
        } catch (Throwable t) {
            text = t.getClass().getName() + ": " + t.getMessage();
            if (t.getCause() != null) text += " (cause " + t.getCause().getClass().getName() + ")";
        }
        System.out.println(label + " -> " + text);
    }

    static MethodHandles.Lookup lk(Class<?> c) throws IllegalAccessException {
        return MethodHandles.privateLookupIn(c, MethodHandles.lookup());
    }

    public static void main(String[] args) throws Exception {
        Loader l = new Loader();
        String o1 = PKG + "O1", g1 = PKG + "G1";
        l.add("O1", List.of());
        l.add("G1", List.of(new Inner(g1, o1, "G1")));
        String o2 = PKG + "O2", m2 = PKG + "M2";
        l.add("O2", List.of(new Inner(m2, o2, "M2")));
        l.add("M2", List.of(new Inner(m2, o2, "M2")));
        String g3 = PKG + "G3";
        l.add("G3", List.of(new Inner(g3, PKG + "Missing", "G3")));
        String o4 = PKG + "O4", g4 = PKG + "G4";
        l.add("O4", List.of(new Inner(g4, PKG + "Other", "G4")));
        l.add("G4", List.of(new Inner(g4, o4, "G4")));
        String o5 = PKG + "O5", t5 = PKG + "T5";
        l.add("O5", List.of(new Inner(t5, o5, "T5")));
        l.add("T5", List.of());

        Class<?> O1 = l.loadClass("r21spoof.O1"), G1 = l.loadClass("r21spoof.G1");
        Class<?> O2 = l.loadClass("r21spoof.O2"), M2 = l.loadClass("r21spoof.M2");
        Class<?> G3 = l.loadClass("r21spoof.G3");
        Class<?> O4 = l.loadClass("r21spoof.O4"), G4 = l.loadClass("r21spoof.G4");
        Class<?> O5 = l.loadClass("r21spoof.O5"), T5 = l.loadClass("r21spoof.T5");

        row("G1.modes", () -> lk(G1).lookupModes());
        row("G1.getDeclaringClass", G1::getDeclaringClass);
        row("G1.getEnclosingClass", G1::getEnclosingClass);
        row("G1.in(O1)", () -> lk(G1).in(O1).lookupModes());
        row("O1.in(G1)", () -> lk(O1).in(G1).lookupModes());
        row("G1.dropPRIVATE.in(O1)",
            () -> lk(G1).dropLookupMode(MethodHandles.Lookup.PRIVATE).in(O1).lookupModes());

        row("M2.getDeclaringClass", M2::getDeclaringClass);
        row("M2.in(O2)", () -> lk(M2).in(O2).lookupModes());
        row("O2.in(M2)", () -> lk(O2).in(M2).lookupModes());
        row("O2.getDeclaredClasses.length", () -> O2.getDeclaredClasses().length);

        row("G3.getDeclaringClass", G3::getDeclaringClass);
        row("G3.in(O1)", () -> lk(G3).in(O1).lookupModes());
        row("O1.in(G3)", () -> lk(O1).in(G3).lookupModes());

        row("G4.getDeclaringClass", G4::getDeclaringClass);
        row("G4.in(O4)", () -> lk(G4).in(O4).lookupModes());
        row("O4.in(G4)", () -> lk(O4).in(G4).lookupModes());

        row("T5.getDeclaringClass", T5::getDeclaringClass);
        row("T5.in(O5)", () -> lk(T5).in(O5).lookupModes());
        row("O5.in(T5)", () -> lk(O5).in(T5).lookupModes());
        row("O5.getDeclaredClasses", () -> {
            try {
                return O5.getDeclaredClasses().length;
            } catch (Throwable t) {
                throw new Exception(t.getClass().getName() + ": " + t.getMessage());
            }
        });

        // A cross-package pair never consults the chain (PACKAGE drops first).
        row("G1.in(Object)", () -> lk(G1).in(Object.class).lookupModes());
    }
}
