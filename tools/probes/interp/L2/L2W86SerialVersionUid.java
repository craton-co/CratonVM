// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 86, lane L2 -- the DEFAULT serialVersionUID
// (`ObjectStreamClass.computeDefaultSUID`, a SHA-1 over the class's name,
// modifiers, sorted interfaces, non-private-static/transient fields, a
// `<clinit>` marker, non-private constructors and methods) and what computing
// it does.
//
//   suid-plain / suid-iface / suid-assert  the hash of three fixed shapes. The
//       values were computed outside any JVM from the JDK 25 algorithm over
//       the members javac 25 emits for the classes at the bottom of this file
//       (no -g needed): `suid-assert` covers javac's synthetic static
//       `$assertionsDisabled` and its `<clinit>`, `suid-iface` the interface
//       ABSTRACT compensation and a constant field (no `<clinit>`).
//   suid-record / suid-enum / suid-explicit  0, 0 and the declared value.
//   init-*: `ObjectStreamClass.hasStaticInitializer` is a JNI native whose
//       `GetStaticMethodID(cl, "<clinit>", "()V")` INITIALIZES the class
//       (HotSpot `get_method_id`). A class with no valid serialization
//       constructor is not initialized by `lookup` (no accessor is built), so
//       `getSerialVersionUID()` is what runs its static initializer -- and a
//       throwing one makes it throw ExceptionInInitializerError, then
//       NoClassDefFoundError. The VM's native answered from the declared
//       methods and ran nothing (wave 86 fix).
//
// Run (stdout must be the block below, byte for byte):
//   $J/javac -d out tools/probes/interp/L2/L2W86SerialVersionUid.java
//   $B --java-home $J/.. -cp out L2W86SerialVersionUid
//   $B --compatible --java-home $J/.. -cp out L2W86SerialVersionUid
//   $J/java -cp out L2W86SerialVersionUid
//
// HOTSPOT_EXPECTED_BEGIN
// suid-plain -3382745865359344030
// suid-iface -3084270939886096296
// suid-assert -5024394253364411519
// suid-record 0
// suid-enum 0
// suid-explicit 42
// init-after-lookup null
// init-after-suid ran
// boom-suid java.lang.ExceptionInInitializerError cause=java.lang.IllegalStateException: boom
// boom-again java.lang.NoClassDefFoundError: Could not initialize class L2W86SuidBoom
// HOTSPOT_EXPECTED_END
import java.io.ObjectStreamClass;
import java.io.Serializable;

public class L2W86SerialVersionUid {
    record Pt(int x, int y) implements Serializable {
    }

    enum Color {
        RED, GREEN
    }

    static final class Explicit implements Serializable {
        private static final long serialVersionUID = 42L;
        int v;
    }

    static long suid(Class<?> c) {
        return ObjectStreamClass.lookup(c).getSerialVersionUID();
    }

    public static void main(String[] args) {
        System.out.println("suid-plain " + suid(L2W86SuidPlain.class));
        System.out.println("suid-iface " + suid(L2W86SuidIface.class));
        System.out.println("suid-assert " + suid(L2W86SuidAssert.class));
        System.out.println("suid-record " + suid(Pt.class));
        System.out.println("suid-enum " + suid(Color.class));
        System.out.println("suid-explicit " + suid(Explicit.class));

        ObjectStreamClass lazy = ObjectStreamClass.lookup(L2W86SuidLazy.class);
        System.out.println("init-after-lookup " + System.getProperty("l2w86.lazy"));
        lazy.getSerialVersionUID();
        System.out.println("init-after-suid " + System.getProperty("l2w86.lazy"));

        ObjectStreamClass boom = ObjectStreamClass.lookup(L2W86SuidBoom.class);
        try {
            boom.getSerialVersionUID();
            System.out.println("boom-suid no-throw");
        } catch (Throwable t) {
            System.out.println("boom-suid " + t.getClass().getName() + " cause=" + t.getCause());
        }
        try {
            ObjectStreamClass.lookup(L2W86SuidBoom.class).getSerialVersionUID();
            System.out.println("boom-again no-throw");
        } catch (Throwable t) {
            System.out.println("boom-again " + t);
        }
    }
}

class L2W86SuidPlain implements Serializable {
    int a;
    private int b;
    transient int c;
    static int d;
    private static int e;
    public String s;

    L2W86SuidPlain() {
    }

    public void m(int x) {
    }

    private void p() {
    }

    static long q(String s, int[] a) {
        return 0L;
    }
}

interface L2W86SuidIface extends Serializable {
    int K = 1;

    void run();
}

class L2W86SuidAssert implements Serializable {
    int x;

    void check(int v) {
        assert v > 0 : "v";
    }
}

// No valid serialization constructor: the first non-serializable superclass
// has no no-arg constructor, so `lookup` builds no accessor and initializes
// nothing.
class L2W86SuidNoCtorBase {
    L2W86SuidNoCtorBase(int x) {
    }
}

class L2W86SuidLazy extends L2W86SuidNoCtorBase implements Serializable {
    static {
        System.setProperty("l2w86.lazy", "ran");
    }

    L2W86SuidLazy() {
        super(1);
    }
}

class L2W86SuidBoom extends L2W86SuidNoCtorBase implements Serializable {
    static {
        if (true) {
            throw new IllegalStateException("boom");
        }
    }

    L2W86SuidBoom() {
        super(1);
    }
}
