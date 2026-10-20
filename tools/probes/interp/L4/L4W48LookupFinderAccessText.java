// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 48, lane L4: the text of a `Lookup` finder's
// access refusal (`docs/internal/fixed-bugs/interpreter-L4-lookup-finder-access-refusals-are-not-hotspots-text-FIXED-20261012.md`).
// CratonVM answered every row with its own `no access: <method|field> C.name
// (modifiers 0x....) from Lookup with modes 0x....`; HotSpot builds the text
// in `MemberName.makeAccessException`. `@H` masks the module's identity hash.
//
// Positive control: row `pub-private-static` (the base printed `no access:
// method L4W48LookupFinderAccessText.secret (modifiers 0x000a) ...`).
//
// Run: javac -d out L4W48LookupFinderAccessText.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible | --jdk-only] -cp out L4W48LookupFinderAccessText
//
// Expected HotSpot 25 output (default and -Xint, identical):
//   pub-private-static: IllegalAccessException: no such method: L4W48LookupFinderAccessText.secret()String/invokeStatic
//   pub-package-static: IllegalAccessException: no such method: L4W48LookupFinderAccessText.pkg()String/invokeStatic
//   pub-protected-static: IllegalAccessException: no such method: L4W48LookupFinderAccessText.protStatic()String/invokeStatic
//   pub-protected-virtual: IllegalAccessException: no such method: L4W48LookupFinderAccessText.prot()String/invokeVirtual
//   pub-private-virtual: IllegalAccessException: no such method: L4W48LookupFinderAccessText.privInst()String/invokeVirtual
//   pub-public-virtual: ok
//   pub-private-ctor: IllegalAccessException: no such constructor: L4W48LookupFinderAccessText.<init>(int)void/newInvokeSpecial
//   pub-package-ctor: IllegalAccessException: no such constructor: L4W48LookupFinderAccessText.<init>(String)void/newInvokeSpecial
//   pub-private-getter: IllegalAccessException: member is private: L4W48LookupFinderAccessText.pf/int/getField, from public Lookup
//   pub-private-setter: IllegalAccessException: member is private: L4W48LookupFinderAccessText.pf/int/putField, from public Lookup
//   pub-protected-getter: IllegalAccessException: member is protected: L4W48LookupFinderAccessText.prf/int/getField, from public Lookup
//   pub-package-staticgetter: IllegalAccessException: member is private to package: L4W48LookupFinderAccessText.spf/int/getStatic, from public Lookup
//   pub-private-staticsetter: IllegalAccessException: member is private: L4W48LookupFinderAccessText.psf/int/putStatic, from public Lookup
//   pub-private-varhandle: IllegalAccessException: member is private: L4W48LookupFinderAccessText.pf/int/getField, from public Lookup
//   pub-package-staticvarhandle: IllegalAccessException: member is private to package: L4W48LookupFinderAccessText.spf/int/getStatic, from public Lookup
//   pkg-private-static: IllegalAccessException: member is private: L4W48LookupFinderAccessText.secret()String/invokeStatic, from class L4W48LookupFinderAccessText (unnamed module @H)
//   pkg-private-virtual: IllegalAccessException: member is private: L4W48LookupFinderAccessText.privInst()String/invokeSpecial, from class L4W48LookupFinderAccessText (unnamed module @H)
//   pkg-private-ctor: IllegalAccessException: member is private: L4W48LookupFinderAccessText.<init>(int)void/invokeSpecial, from class L4W48LookupFinderAccessText (unnamed module @H)
//   pkg-private-getter: IllegalAccessException: member is private: L4W48LookupFinderAccessText.pf/int/getField, from class L4W48LookupFinderAccessText (unnamed module @H)
//   pkg-private-setter: IllegalAccessException: member is private: L4W48LookupFinderAccessText.pf/int/putField, from class L4W48LookupFinderAccessText (unnamed module @H)
//   pkg-private-staticgetter: IllegalAccessException: member is private: L4W48LookupFinderAccessText.psf/int/getStatic, from class L4W48LookupFinderAccessText (unnamed module @H)
//   pkg-private-varhandle: IllegalAccessException: member is private: L4W48LookupFinderAccessText.pf/int/getField, from class L4W48LookupFinderAccessText (unnamed module @H)
//   pkg-package-static: ok
//   mod-package-static: IllegalAccessException: member is private to package: L4W48LookupFinderAccessText.pkg()String/invokeStatic, from class L4W48LookupFinderAccessText (unnamed module @H)
//   mod-protected-static: IllegalAccessException: member is protected: L4W48LookupFinderAccessText.protStatic()String/invokeStatic, from class L4W48LookupFinderAccessText (unnamed module @H)
//   mod-protected-virtual: IllegalAccessException: member is protected: L4W48LookupFinderAccessText.prot()String/invokeVirtual, from class L4W48LookupFinderAccessText (unnamed module @H)
//   mod-package-getter: IllegalAccessException: member is private to package: L4W48LookupFinderAccessText.spf/int/getStatic, from class L4W48LookupFinderAccessText (unnamed module @H)
//   mod-protected-getter: IllegalAccessException: member is protected: L4W48LookupFinderAccessText.prf/int/getField, from class L4W48LookupFinderAccessText (unnamed module @H)
//   mod-package-ctor: IllegalAccessException: member is private to package: L4W48LookupFinderAccessText.<init>(String)void/invokeSpecial, from class L4W48LookupFinderAccessText (unnamed module @H)
//   mod-private-static: IllegalAccessException: member is private: L4W48LookupFinderAccessText.secret()String/invokeStatic, from class L4W48LookupFinderAccessText (unnamed module @H)
//   none-public-virtual: IllegalAccessException: symbolic reference class is not accessible: class L4W48LookupFinderAccessText, from class L4W48LookupFinderAccessText (unnamed module @H)
//   none-private-static: IllegalAccessException: symbolic reference class is not accessible: class L4W48LookupFinderAccessText, from class L4W48LookupFinderAccessText (unnamed module @H)
//   none-private-getter: IllegalAccessException: symbolic reference class is not accessible: class L4W48LookupFinderAccessText, from class L4W48LookupFinderAccessText (unnamed module @H)
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

public class L4W48LookupFinderAccessText {
    private static String secret() { return "s"; }
    static String pkg() { return "p"; }
    protected String prot() { return "r"; }
    protected static String protStatic() { return "ps"; }
    private String privInst() { return "i"; }
    public String pubInst() { return "u"; }
    private int pf;
    static int spf;
    protected int prf;
    private static int psf;

    private L4W48LookupFinderAccessText(int x) { }
    L4W48LookupFinderAccessText(String x) { }
    public L4W48LookupFinderAccessText() { }

    interface Body {
        Object run() throws Throwable;
    }

    static void row(String name, Body body) {
        String out;
        try {
            out = String.valueOf(body.run() != null ? "ok" : "null");
        } catch (Throwable t) {
            String m = String.valueOf(t.getMessage()).replaceAll("@[0-9a-f]+", "@H");
            out = t.getClass().getSimpleName() + ": " + m;
        }
        System.out.println(name + ": " + out);
    }

    public static void main(String[] args) throws Throwable {
        Class<?> c = L4W48LookupFinderAccessText.class;
        MethodType s = MethodType.methodType(String.class);
        MethodHandles.Lookup pub = MethodHandles.publicLookup();
        MethodHandles.Lookup pkg = MethodHandles.lookup().dropLookupMode(MethodHandles.Lookup.PRIVATE);
        MethodHandles.Lookup mod = MethodHandles.lookup().dropLookupMode(MethodHandles.Lookup.PACKAGE);
        MethodHandles.Lookup none = MethodHandles.lookup().dropLookupMode(MethodHandles.Lookup.PUBLIC);

        row("pub-private-static", () -> pub.findStatic(c, "secret", s));
        row("pub-package-static", () -> pub.findStatic(c, "pkg", s));
        row("pub-protected-static", () -> pub.findStatic(c, "protStatic", s));
        row("pub-protected-virtual", () -> pub.findVirtual(c, "prot", s));
        row("pub-private-virtual", () -> pub.findVirtual(c, "privInst", s));
        row("pub-public-virtual", () -> pub.findVirtual(c, "pubInst", s));
        row("pub-private-ctor", () -> pub.findConstructor(c, MethodType.methodType(void.class, int.class)));
        row("pub-package-ctor", () -> pub.findConstructor(c, MethodType.methodType(void.class, String.class)));
        row("pub-private-getter", () -> pub.findGetter(c, "pf", int.class));
        row("pub-private-setter", () -> pub.findSetter(c, "pf", int.class));
        row("pub-protected-getter", () -> pub.findGetter(c, "prf", int.class));
        row("pub-package-staticgetter", () -> pub.findStaticGetter(c, "spf", int.class));
        row("pub-private-staticsetter", () -> pub.findStaticSetter(c, "psf", int.class));
        row("pub-private-varhandle", () -> pub.findVarHandle(c, "pf", int.class));
        row("pub-package-staticvarhandle", () -> pub.findStaticVarHandle(c, "spf", int.class));

        row("pkg-private-static", () -> pkg.findStatic(c, "secret", s));
        row("pkg-private-virtual", () -> pkg.findVirtual(c, "privInst", s));
        row("pkg-private-ctor", () -> pkg.findConstructor(c, MethodType.methodType(void.class, int.class)));
        row("pkg-private-getter", () -> pkg.findGetter(c, "pf", int.class));
        row("pkg-private-setter", () -> pkg.findSetter(c, "pf", int.class));
        row("pkg-private-staticgetter", () -> pkg.findStaticGetter(c, "psf", int.class));
        row("pkg-private-varhandle", () -> pkg.findVarHandle(c, "pf", int.class));
        row("pkg-package-static", () -> pkg.findStatic(c, "pkg", s));

        row("mod-package-static", () -> mod.findStatic(c, "pkg", s));
        row("mod-protected-static", () -> mod.findStatic(c, "protStatic", s));
        row("mod-protected-virtual", () -> mod.findVirtual(c, "prot", s));
        row("mod-package-getter", () -> mod.findStaticGetter(c, "spf", int.class));
        row("mod-protected-getter", () -> mod.findGetter(c, "prf", int.class));
        row("mod-package-ctor", () -> mod.findConstructor(c, MethodType.methodType(void.class, String.class)));
        row("mod-private-static", () -> mod.findStatic(c, "secret", s));

        row("none-public-virtual", () -> none.findVirtual(c, "pubInst", s));
        row("none-private-static", () -> none.findStatic(c, "secret", s));
        row("none-private-getter", () -> none.findGetter(c, "pf", int.class));
    }
}
