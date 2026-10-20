// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 65, lane L4 -- a bound field handle keeps its
// owner and its minted slot (`CRATONVM_MH_BOUND_FIELD_OWNER`, default on, kill
// switch `=0`; proposal
// `i64-L4-proposal-bound-field-handles-keep-owner-and-slot-20261028.md`).
//
// Two loaders each define a class named `L4W65BoundFieldOwner$BoxN` from
// DIFFERENT bytes: loader A's is `BoxA` renamed (`int a; int v;`), loader B's
// is `BoxB` renamed (`int v; int pad;`), so `v` sits in another slot of each
// layout. `findGetter(BoxN_A, "v").bindTo(a)` must read A's `v` of `a`, and
// `findGetter(BoxN_B, "v").bindTo(b)` B's of `b`. Before the switch this
// model's bound `GETTER` / `SETTER` arm resolved the slot by NAME
// (`MH_CLASS`), which finds ONE of the two classes: one of the two bound
// readers read the other layout's slot (`a.a` or `b.pad`) and one bound
// setter wrote it. The unbound rows are controls (stage 2 resolves those by
// the recorded owner's id already).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// namesakes: true true
// unbound-a: 7
// unbound-b: 9
// bound-a: 7
// bound-b: 9
// bound-a-exact: 7
// bound-b-exact: 9
// bound-a-astype: 7
// bound-b-unreflect: 9
// bound-setter-a: 11/3
// bound-setter-b: 12/5
// bound-foreign: java.lang.ClassCastException: Cannot cast L4W65BoundFieldOwner$BoxN to L4W65BoundFieldOwner$BoxN
// bound-type: ()int
// warm: 46000
// after-bound-a: 11
// after-bound-b: 12
// after-bound-setter-b: 13/5
// HOTSPOT_EXPECTED_END
//
// Switches: under `--compatible` the bound rows need
// `CRATONVM_MH_BOUND_FIELD_OWNER` (default on) on top of
// `CRATONVM_MH_OWNER_ID_AUTHORITY` (default on); `=0` is the positive control
// (one of `bound-a` / `bound-b` prints the other layout's slot: `3` or `5`).
// Run also with `CRATONVM_MH_FIELD_SLOT_FROM_MINT=1` (the bound copy then
// carries the minted slot too): no row may move. Under `--jdk-only` the
// finders answer the JDK's own `DirectMethodHandle`s.
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L4W65BoundFieldOwner`.
import java.io.IOException;
import java.io.InputStream;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.nio.charset.StandardCharsets;

public class L4W65BoundFieldOwner {
    public static class BoxA {
        public int a = 3;
        public int v = 7;
    }

    public static class BoxB {
        public int v = 9;
        public int pad = 5;
    }

    static final String BOX_A = "L4W65BoundFieldOwner$BoxA";
    static final String BOX_B = "L4W65BoundFieldOwner$BoxB";
    /** The name both loaders define; as long as the two source names. */
    static final String BOX_N = "L4W65BoundFieldOwner$BoxN";

    static final class Definer extends ClassLoader {
        Definer(ClassLoader parent) {
            super(parent);
        }

        Class<?> define(byte[] bytes) {
            return defineClass(BOX_N, bytes, 0, bytes.length);
        }
    }

    /** `from`'s class bytes with every occurrence of its name renamed to `BOX_N`. */
    static byte[] renamed(String from) throws IOException {
        byte[] bytes;
        try (InputStream in = L4W65BoundFieldOwner.class.getResourceAsStream(from + ".class")) {
            if (in == null) {
                throw new IOException("no class file for " + from);
            }
            bytes = in.readAllBytes();
        }
        byte[] needle = from.getBytes(StandardCharsets.UTF_8);
        byte[] with = BOX_N.getBytes(StandardCharsets.UTF_8);
        for (int i = 0; i + needle.length <= bytes.length; i++) {
            boolean hit = true;
            for (int j = 0; j < needle.length && hit; j++) {
                hit = bytes[i + j] == needle[j];
            }
            if (hit) {
                System.arraycopy(with, 0, bytes, i, with.length);
            }
        }
        return bytes;
    }

    interface Row {
        Object run() throws Throwable;
    }

    static void row(String name, Row r) {
        String out;
        try {
            out = String.valueOf(r.run());
        } catch (Throwable t) {
            out = String.valueOf(t);
        }
        if (out.startsWith("MethodHandle(")) {
            out = "MethodHandle(...)";
        }
        System.out.println(name + ": " + out);
    }

    public static void main(String[] args) throws Throwable {
        ClassLoader app = L4W65BoundFieldOwner.class.getClassLoader();
        Class<?> boxA = new Definer(app).define(renamed(BOX_A));
        Class<?> boxB = new Definer(app).define(renamed(BOX_B));
        Object a = boxA.getConstructor().newInstance();
        Object b = boxB.getConstructor().newInstance();

        MethodHandles.Lookup lk = MethodHandles.lookup();
        MethodHandle getA = lk.findGetter(boxA, "v", int.class);
        MethodHandle getB = lk.findGetter(boxB, "v", int.class);
        MethodHandle setA = lk.findSetter(boxA, "v", int.class);
        MethodHandle setB = lk.findSetter(boxB, "v", int.class);
        MethodHandle getAa = lk.findGetter(boxA, "a", int.class);
        MethodHandle getBpad = lk.findGetter(boxB, "pad", int.class);
        MethodHandle ugetB = lk.unreflectGetter(boxB.getField("v"));

        row("namesakes", () -> boxA.getName().equals(boxB.getName()) + " " + (boxA != boxB));
        row("unbound-a", () -> (int) getA.invoke(a));
        row("unbound-b", () -> (int) getB.invoke(b));
        row("bound-a", () -> (int) getA.bindTo(a).invoke());
        row("bound-b", () -> (int) getB.bindTo(b).invoke());
        row("bound-a-exact", () -> (int) getA.bindTo(a).invokeExact());
        row("bound-b-exact", () -> (int) getB.bindTo(b).invokeExact());
        row("bound-a-astype", () -> getA.bindTo(a).asType(MethodType.methodType(Object.class)).invoke());
        row("bound-b-unreflect", () -> (int) ugetB.bindTo(b).invoke());
        row("bound-setter-a", () -> {
            setA.bindTo(a).invoke(11);
            return (int) getA.invoke(a) + "/" + (int) getAa.invoke(a);
        });
        row("bound-setter-b", () -> {
            setB.bindTo(b).invoke(12);
            return (int) getB.invoke(b) + "/" + (int) getBpad.invoke(b);
        });
        row("bound-foreign", () -> getA.bindTo(b));
        MethodHandle boundA = getA.bindTo(a);
        MethodHandle boundB = getB.bindTo(b);
        row("bound-type", () -> boundA.type());
        long n = 0;
        for (int i = 0; i < 2000; i++) {
            n += (int) boundA.invoke();
            n += (int) boundB.invokeExact();
        }
        System.out.println("warm: " + n);
        row("after-bound-a", () -> (int) boundA.invoke());
        row("after-bound-b", () -> (int) boundB.invoke());
        MethodHandle boundSetB = setB.bindTo(b);
        row("after-bound-setter-b", () -> {
            boundSetB.invoke(13);
            return (int) getB.invoke(b) + "/" + (int) getBpad.invoke(b);
        });
    }
}
