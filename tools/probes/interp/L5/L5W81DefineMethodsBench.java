// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3, wave 81, lane L5 -- what a user-loader define pays for
// the classes' METHODS
// (`docs/internal/fixed-bugs/interpreter-i72-L5-proposal-define-time-type-maps-at-link-DONE-20261115.md`,
// "Progress (wave 81)"). The W71 / W72 benches define classes with no methods,
// so the define-time type-map walk (every method of every user-loader class,
// under the class-manager write lock) never shows there. Here every class has
// 24 straight-line static methods (20 int, 4 reference):
//
//   define-plain        control: a fresh `ClassLoader` subclass per round
//                       defines CLASSES classes with no methods.
//   define-methods      the same classes with their methods, defined and
//                       never initialised: the walk is the difference to
//                       define-plain (CRATONVM_DEFINE_TYPE_MAPS_AT_LINK
//                       should take it away: HotSpot verifies at link).
//   define-methods-init `Class.forName(name, true, loader)` of a fresh loader:
//                       the platform delegation miss, define, link, init.
//                       The switch moves the walk to the link here, so this
//                       row should not drop with it (the walk now runs under
//                       a READ lock).
//
// Every row reports `<row> ns/class=<median of rounds 1..ROUNDS-1>` and
// `<row> first ns/class=<round 0>`.
//
// Run ($B the cratonvm binary, $J the JDK 25 bin directory; interleave the
// arms, five repetitions, medians):
//   $J/javac -d out L5W81DefineMethodsBench.java
//   env CRATONVM_NONE=1                     $B --java-home $J/.. -cp out L5W81DefineMethodsBench
//   env CRATONVM_DEFINE_TYPE_MAPS_AT_LINK=1 $B --java-home $J/.. -cp out L5W81DefineMethodsBench
//   $J/java -cp out L5W81DefineMethodsBench
// (each also with `--compatible` and with `--nojit`). One row only:
// `L5W81DefineMethodsBench <row>`.
//
// stdout lines without "ns/" are deterministic (the same on every VM):
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// classes-per-round=400 methods-per-class=24
// define-plain defined=400
// define-methods defined=400
// define-methods-init initialized=400
// invoke m5(3,4)=17 s2(x)=x
// HOTSPOT_EXPECTED_END

import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.lang.reflect.Method;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.HashMap;
import java.util.List;
import java.util.Map;

public class L5W81DefineMethodsBench {
    static final int CLASSES = 400;
    static final int INT_METHODS = 20;
    static final int STRING_METHODS = 4;
    static final int METHODS = INT_METHODS + STRING_METHODS;
    static final int ROUNDS = 7;

    /**
     * `public class <name>` (version 49) with `static int m<k>(int a, int b)
     * { return a * b + k; }` for k < INT_METHODS and `static String s<k>(String
     * s) { return s; }` for k < STRING_METHODS, or no methods at all.
     */
    static byte[] classBytes(String internal, boolean methods) throws IOException {
        List<String> utf8 = new ArrayList<>();
        utf8.add(internal);          // 1 (2: Class #1)
        utf8.add("java/lang/Object"); // 3 (4: Class #3)
        utf8.add("Code");            // 5
        utf8.add("(II)I");           // 6
        utf8.add("(Ljava/lang/String;)Ljava/lang/String;"); // 7
        int firstName = 8;
        if (methods) {
            for (int k = 0; k < INT_METHODS; k++) {
                utf8.add("m" + k);
            }
            for (int k = 0; k < STRING_METHODS; k++) {
                utf8.add("s" + k);
            }
        }
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        DataOutputStream out = new DataOutputStream(bytes);
        out.writeInt(0xCAFEBABE);
        out.writeShort(0);
        out.writeShort(49);
        // Two Class entries besides the Utf8s.
        out.writeShort(utf8.size() + 2 + 1);
        out.writeByte(1); out.writeUTF(utf8.get(0));
        out.writeByte(7); out.writeShort(1);
        out.writeByte(1); out.writeUTF(utf8.get(1));
        out.writeByte(7); out.writeShort(3);
        for (int i = 2; i < utf8.size(); i++) {
            out.writeByte(1); out.writeUTF(utf8.get(i));
        }
        out.writeShort(0x21);
        out.writeShort(2);
        out.writeShort(4);
        out.writeShort(0); // interfaces
        out.writeShort(0); // fields
        out.writeShort(methods ? METHODS : 0);
        if (methods) {
            for (int k = 0; k < INT_METHODS; k++) {
                out.writeShort(0x0009);
                out.writeShort(firstName + k);
                out.writeShort(6);
                out.writeShort(1);
                out.writeShort(5);
                out.writeInt(20);
                out.writeShort(2); // max_stack
                out.writeShort(2); // max_locals
                out.writeInt(8);
                out.writeByte(0x1a); // iload_0
                out.writeByte(0x1b); // iload_1
                out.writeByte(0x68); // imul
                out.writeByte(0x11); // sipush k
                out.writeShort(k);
                out.writeByte(0x60); // iadd
                out.writeByte(0xac); // ireturn
                out.writeShort(0);
                out.writeShort(0);
            }
            for (int k = 0; k < STRING_METHODS; k++) {
                out.writeShort(0x0009);
                out.writeShort(firstName + INT_METHODS + k);
                out.writeShort(7);
                out.writeShort(1);
                out.writeShort(5);
                out.writeInt(14);
                out.writeShort(1); // max_stack
                out.writeShort(1); // max_locals
                out.writeInt(2);
                out.writeByte(0x2a); // aload_0
                out.writeByte(0xb0); // areturn
                out.writeShort(0);
                out.writeShort(0);
            }
        }
        out.writeShort(0); // class attributes
        out.flush();
        return bytes.toByteArray();
    }

    static final List<String> NAMES = new ArrayList<>();
    static final Map<String, byte[]> PLAIN = new HashMap<>();
    static final Map<String, byte[]> WITH_METHODS = new HashMap<>();

    /** A user-defined loader (parent: platform) defining from memory. */
    static final class MemLoader extends ClassLoader {
        final Map<String, byte[]> bytes;

        MemLoader(Map<String, byte[]> bytes) {
            super(ClassLoader.getPlatformClassLoader());
            this.bytes = bytes;
        }

        Class<?> defineDirect(String name) {
            byte[] b = bytes.get(name);
            return defineClass(name, b, 0, b.length);
        }

        @Override
        protected Class<?> findClass(String name) throws ClassNotFoundException {
            byte[] b = bytes.get(name);
            if (b == null) {
                throw new ClassNotFoundException(name);
            }
            return defineClass(name, b, 0, b.length);
        }
    }

    interface Round {
        long run() throws Exception;
    }

    interface RoundFactory {
        Round make() throws Exception;
    }

    static void row(String label, String countLabel, RoundFactory factory) throws Exception {
        long[] perClass = new long[ROUNDS];
        long count = 0;
        for (int r = 0; r < ROUNDS; r++) {
            Round round = factory.make();
            long t0 = System.nanoTime();
            count = round.run();
            perClass[r] = (System.nanoTime() - t0) / CLASSES;
        }
        long[] later = Arrays.copyOfRange(perClass, 1, ROUNDS);
        Arrays.sort(later);
        System.out.println(label + " " + countLabel + "=" + count);
        System.out.println(label + " first ns/class=" + perClass[0]);
        System.out.println(label + " ns/class=" + later[later.length / 2]);
    }

    static Round defineAll(Map<String, byte[]> bytes) {
        MemLoader loader = new MemLoader(bytes);
        return () -> {
            long n = 0;
            for (String name : NAMES) {
                if (loader.defineDirect(name).getClassLoader() == loader) {
                    n++;
                }
            }
            return n;
        };
    }

    public static void main(String[] args) throws Exception {
        String only = args.length > 0 ? args[0] : null;
        for (int i = 0; i < CLASSES; i++) {
            String name = "w81m.D" + i;
            String internal = name.replace('.', '/');
            NAMES.add(name);
            PLAIN.put(name, classBytes(internal, false));
            WITH_METHODS.put(name, classBytes(internal, true));
        }
        System.out.println("classes-per-round=" + CLASSES + " methods-per-class=" + METHODS);

        if (only == null || only.equals("define-plain")) {
            row("define-plain", "defined", () -> defineAll(PLAIN));
        }
        if (only == null || only.equals("define-methods")) {
            row("define-methods", "defined", () -> defineAll(WITH_METHODS));
        }
        if (only == null || only.equals("define-methods-init")) {
            row("define-methods-init", "initialized", () -> {
                MemLoader loader = new MemLoader(WITH_METHODS);
                return () -> {
                    long n = 0;
                    for (String name : NAMES) {
                        if (Class.forName(name, true, loader).getClassLoader() == loader) {
                            n++;
                        }
                    }
                    return n;
                };
            });
        }
        // The linked class runs: one call of each method shape.
        MemLoader loader = new MemLoader(WITH_METHODS);
        Class<?> c = Class.forName(NAMES.get(7), true, loader);
        Method m5 = c.getDeclaredMethod("m5", int.class, int.class);
        Method s2 = c.getDeclaredMethod("s2", String.class);
        System.out.println("invoke m5(3,4)=" + m5.invoke(null, 3, 4) + " s2(x)=" + s2.invoke(null, "x"));
    }
}
