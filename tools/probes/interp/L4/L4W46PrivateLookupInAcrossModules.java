// Interpreter round i1, wave 46, lane L4 -- `MethodHandles.privateLookupIn`
// of a target in ANOTHER module answers the cross-module shape: MODULE
// dropped (modes 15) and the caller's lookup class as the previous lookup
// class (`docs/internal/fixed-bugs/interpreter-L5-privatelookupin-admits-a-target-whose-package-is-not-open-FIXED-20261010.md`).
//
// Rows (the caller is this class, in the app loader's unnamed module):
//   same          a class of this module: 31, no previous lookup class
//   other-loader  a class a user loader defined (its own unnamed module):
//                 15, this class as the previous lookup class
//   to-string     that lookup's toString (target/previous)
//   full-priv     hasFullPrivilegeAccess / hasPrivateAccess of that lookup
//   define        defineClass through it (PACKAGE access is kept)
//   hidden        defineHiddenClass through it (needs MODULE: refused)
//   in-back       .in(this class) from it
//
// Run (no setup):
//   javac -d out L4W46PrivateLookupInAcrossModules.java
//   cratonvm --java-home <jdk25> [--nojit] -cp out L4W46PrivateLookupInAcrossModules
//
// Expected HotSpot 25.0.3 output (`java` and `-Xint`, identical):
//   same=31 null
//   other-loader=15 L4W46PrivateLookupInAcrossModules
//   to-string=w46.Host/L4W46PrivateLookupInAcrossModules
//   full-priv=false false
//   define=w46.Def2 true
//   hidden=java.lang.IllegalAccessException: w46.Host/L4W46PrivateLookupInAcrossModules does not have full privilege access
//   in-back=L4W46PrivateLookupInAcrossModules 1 w46.Host
//
// On the base `55834015b` (from the code, every mode): the live
// `privateLookupIn` native (`lang_invoke.rs`) always wrote modes 0x1F and a
// null previous lookup class, so `other-loader=31 null`,
// `to-string=w46.Host`, `full-priv=true true`, and `hidden` defines the class
// (prints `ok`). `--compatible` keeps those answers by design (it asks no
// module question). `hasPrivateAccess` answered `PRIVATE` alone
// (`classloader::lk_has_private_access`); the JDK's is
// `hasFullPrivilegeAccess()`, which only a cross-module lookup tells apart.
//
// Positive control (`--jdk-only`): `CRATONVM_DBG_ACCESS=1` prints
// `[ACCESS-DBG] privateLookupIn across modules: modes 15, previous lookup
// class set` once per cross-module row (`other-loader`); the base has no such
// line.

import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodHandles.Lookup;

public class L4W46PrivateLookupInAcrossModules {
    static byte[] def(String name) {
        // A minimal class file: public class <name> extends Object, no members.
        java.io.ByteArrayOutputStream buf = new java.io.ByteArrayOutputStream();
        java.io.DataOutputStream out = new java.io.DataOutputStream(buf);
        try {
            out.writeInt(0xCAFEBABE);
            out.writeShort(0);
            out.writeShort(52);
            out.writeShort(5);
            out.writeByte(1); out.writeUTF(name);
            out.writeByte(7); out.writeShort(1);
            out.writeByte(1); out.writeUTF("java/lang/Object");
            out.writeByte(7); out.writeShort(3);
            out.writeShort(0x21);
            out.writeShort(2);
            out.writeShort(4);
            out.writeShort(0);
            out.writeShort(0);
            out.writeShort(0);
            out.writeShort(0);
        } catch (java.io.IOException e) {
            throw new RuntimeException(e);
        }
        return buf.toByteArray();
    }

    static final class Own extends ClassLoader {
        Own() { super("w46", L4W46PrivateLookupInAcrossModules.class.getClassLoader()); }
        Class<?> define(String name, byte[] b) { return defineClass(name, b, 0, b.length); }
    }

    interface Row { String run() throws Throwable; }

    static void row(String name, Row r) {
        String out;
        try {
            out = r.run();
        } catch (Throwable t) {
            out = t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(name + "=" + out);
    }

    static String shape(Lookup l) {
        Class<?> prev = l.previousLookupClass();
        return l.lookupModes() + " " + (prev == null ? "null" : prev.getName());
    }

    public static void main(String[] args) throws Throwable {
        Lookup self = MethodHandles.lookup();
        Own loader = new Own();
        Class<?> host = loader.define("w46.Host", def("w46/Host"));
        row("same", () -> shape(MethodHandles.privateLookupIn(L4W46PrivateLookupInAcrossModules.class, self)));
        Lookup across = MethodHandles.privateLookupIn(host, self);
        row("other-loader", () -> shape(across));
        row("to-string", () -> across.toString());
        row("full-priv", () -> across.hasFullPrivilegeAccess() + " " + across.hasPrivateAccess());
        row("define", () -> {
            Class<?> c = across.defineClass(def("w46/Def2"));
            return c.getName() + " " + (c.getClassLoader() == loader);
        });
        row("hidden", () -> {
            across.defineHiddenClass(def("w46/Hid"), false);
            return "ok";
        });
        row("in-back", () -> {
            Lookup back = across.in(L4W46PrivateLookupInAcrossModules.class);
            return back.lookupClass().getName() + " " + shape(back);
        });
    }
}
