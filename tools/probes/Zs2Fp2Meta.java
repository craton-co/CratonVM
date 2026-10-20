// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// zs s2/fp2 (2026-10-14): Java-visible answers of the class metadata whose
// in-VM layout the footprint lane changed
// (docs/internal/zgc-round-zs-20261014/s2-fp2-report.md):
//   - the runtime constant pool's Utf8 slots (string constants: ASCII,
//     non-BMP, embedded NUL, a lone surrogate, long and double constants,
//     literal interning identity across classes);
//   - the class loader's vtable descriptors, now shared by inheritors
//     (overrides at several depths, calls through super-typed references,
//     an inherited abstract slot filled lower down, package-private
//     shadowing in one package);
//   - inner-class metadata (getDeclaredClasses / getSimpleName /
//     getEnclosingClass / isAnonymousClass / modifiers);
//   - a load row over a name list (as Zr7RsBoot / Zs1FpBoot) that walks each
//     class's InnerClasses / nest metadata through reflection.
// Every stdout line is deterministic and must equal HotSpot's byte for byte.
//
// Usage:
//   Zs2Fp2Meta meta                  the fixed checks above
//   Zs2Fp2Meta classes <n> <file>    forName(name, false, null) for the first
//                                    n names of <file> (Zr7RsBoot list, made on
//                                    HotSpot), then a reflective inner-class
//                                    walk of each; prints a checksum.
import java.io.BufferedReader;
import java.io.FileReader;
import java.lang.reflect.Modifier;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Comparator;
import java.util.List;

public class Zs2Fp2Meta {
    // --- constant pool ----------------------------------------------------
    static final String ASCII = "java/lang/Object";
    static final String NONBMP = "x😀y";
    static final String NUL = "a\u0000b";
    static final String LONE = "p\uD800q";
    static final long BIG = 0xDEAD_BEEF_CAFE_BABEL;
    static final double PI = 3.141592653589793;

    static String hex(String s) {
        StringBuilder b = new StringBuilder();
        for (int i = 0; i < s.length(); i++) {
            if (i > 0) {
                b.append(',');
            }
            b.append(Integer.toHexString(s.charAt(i)));
        }
        return b.toString();
    }

    static class Other {
        static String ascii() {
            return "java/lang/Object";
        }
    }

    // --- vtable -------------------------------------------------------------
    abstract static class A {
        String a() {
            return "A.a";
        }

        String b() {
            return "A.b";
        }

        abstract String c();

        String d() {
            return "A.d";
        }
    }

    static class B extends A {
        String b() {
            return "B.b";
        }

        String c() {
            return "B.c";
        }
    }

    static class C extends B {
        String a() {
            return "C.a";
        }

        String e() {
            return "C.e";
        }
    }

    static class D extends C {
        String b() {
            return "D.b(" + super.b() + ")";
        }

        String d() {
            return "D.d(" + super.d() + ")";
        }
    }

    static final class E extends D {
    }

    static String calls(A x) {
        return x.a() + " " + x.b() + " " + x.c() + " " + x.d()
                + (x instanceof C ? " " + ((C) x).e() : "");
    }

    // --- inner classes ------------------------------------------------------
    interface Shape {
        String name();
    }

    static String inner(Class<?> c) {
        StringBuilder b = new StringBuilder();
        b.append(c.getName()).append(" simple=").append(c.getSimpleName());
        b.append(" anon=").append(c.isAnonymousClass());
        b.append(" local=").append(c.isLocalClass());
        b.append(" member=").append(c.isMemberClass());
        Class<?> enc = c.getEnclosingClass();
        b.append(" enclosing=").append(enc == null ? "-" : enc.getName());
        Class<?> dec = c.getDeclaringClass();
        b.append(" declaring=").append(dec == null ? "-" : dec.getName());
        b.append(" mods=").append(Modifier.toString(c.getModifiers()));
        return b.toString();
    }

    static void meta() {
        System.out.println("zs2fp2 cp ascii=" + ASCII + " same=" + (ASCII == Other.ascii()));
        System.out.println("zs2fp2 cp nonbmp=" + hex(NONBMP) + " len=" + NONBMP.length()
                + " cps=" + NONBMP.codePointCount(0, NONBMP.length()));
        System.out.println("zs2fp2 cp nul=" + hex(NUL) + " lone=" + hex(LONE));
        System.out.println("zs2fp2 cp long=" + Long.toHexString(BIG) + " double=" + PI
                + " ldc2=" + (BIG ^ Double.doubleToLongBits(PI)));
        System.out.println("zs2fp2 cp interned=" + ("ab" + "cd" == "abcd")
                + " runtime=" + (new String("abcd").intern() == "abcd"));
        A[] xs = {new B(), new C(), new D(), new E()};
        for (A x : xs) {
            System.out.println("zs2fp2 vt " + x.getClass().getSimpleName() + ": " + calls(x));
        }
        // A call site reached by every receiver (megamorphic), repeated so a
        // compiled caller sees the same answers.
        long h = 0;
        for (int i = 0; i < 20000; i++) {
            h = h * 31 + calls(xs[i & 3]).hashCode();
        }
        System.out.println("zs2fp2 vt loop=" + Long.toHexString(h));
        System.out.println("zs2fp2 vt object=" + new E().equals(new E()) + " "
                + (new B().toString().startsWith("Zs2Fp2Meta$B@")));

        Shape anon = new Shape() {
            public String name() {
                return "anon";
            }
        };
        class Local implements Shape {
            public String name() {
                return "local";
            }
        }
        System.out.println("zs2fp2 ic " + inner(anon.getClass()));
        System.out.println("zs2fp2 ic " + inner(Local.class));
        System.out.println("zs2fp2 ic " + inner(Other.class));
        System.out.println("zs2fp2 ic " + inner(Shape.class));
        System.out.println("zs2fp2 ic " + inner(java.util.Map.Entry.class));
        System.out.println("zs2fp2 ic " + inner(java.util.AbstractMap.SimpleEntry.class));
        Class<?>[] decl = Zs2Fp2Meta.class.getDeclaredClasses();
        Arrays.sort(decl, Comparator.comparing(Class::getName));
        StringBuilder names = new StringBuilder();
        for (Class<?> c : decl) {
            names.append(' ').append(c.getSimpleName());
        }
        System.out.println("zs2fp2 ic declared=" + decl.length + names);
        System.out.println("zs2fp2 ic nest=" + Zs2Fp2Meta.class.getNestHost().getName() + " "
                + B.class.getNestHost().getName() + " " + B.class.isNestmateOf(Other.class));
        System.out.println("zs2fp2 mode=meta done");
    }

    static void classes(int n, String file) throws Exception {
        List<String> names = new ArrayList<>();
        try (BufferedReader r = new BufferedReader(new FileReader(file))) {
            String line;
            while ((line = r.readLine()) != null && names.size() < n) {
                line = line.trim();
                if (!line.isEmpty()) {
                    names.add(line);
                }
            }
        }
        int loaded = 0;
        long check = 0;
        for (String name : names) {
            Class<?> c;
            try {
                c = Class.forName(name, false, null);
            } catch (Throwable t) {
                continue;
            }
            loaded++;
            Class<?>[] decl;
            try {
                decl = c.getDeclaredClasses();
            } catch (Throwable t) {
                check = check * 31 + 7;
                continue;
            }
            Arrays.sort(decl, Comparator.comparing(Class::getName));
            check = check * 31 + decl.length;
            for (Class<?> d : decl) {
                check = check * 31 + d.getSimpleName().hashCode();
                check = check * 31 + d.getModifiers();
            }
            Class<?> enc = c.getEnclosingClass();
            check = check * 31 + (enc == null ? 0 : enc.getName().hashCode());
            check = check * 31 + c.getNestHost().getName().hashCode();
        }
        System.err.println("zs2fp2-classes listed=" + names.size() + " loaded=" + loaded);
        System.out.println("zs2fp2 mode=classes n=" + n + " check=" + Long.toHexString(check) + " done");
    }

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "meta";
        if (mode.equals("classes")) {
            classes(Integer.parseInt(args[1]), args[2]);
        } else {
            meta();
        }
    }
}
