// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 92, lane L2 -- the DEFERRED compiled field site
// (`CRATONVM_JIT_DEFERRED_FIELD_SITES`, default OFF; stage 2 of
// `i50-L1-proposal-compile-doors-resolve-nothing`, the
// `i38-L2-proposal-constant-pool-indexed-field-sites` helper). `hot` is warmed
// with mode 0 until it compiles; every other mode is a COLD branch touching a
// field of a class that is not loaded when it compiles. Before the switch such
// a method was refused (`unresolved-field-site`) and stayed interpreted, so
// these rows exercised the interpreter; with the switch it compiles and each
// cold site runs `jit_field_site_cp` -- resolution, linking checks,
// initialisation and the access at the program point. The output must not
// depend on the switch, the mode or the tier.
//
//   getstatic     a static read whose owner's <clinit> runs at the site
//   putstatic     a static write (its owner initialised first), read back
//   fields        `new` + putfield + getfield of a not-yet-loaded class
//   putfield-npe  a null receiver at a deferred putfield (JEP 358 message)
//   getfield-npe  the same at a deferred getfield
//   wide          a long and a double static (category-2 values)
//   boom1/boom2   a <clinit> that throws: EIIE first, then NCDFE
//   log           the <clinit> order
//
// Compile without -g (the `<parameter3>` spelling depends on it).
//
// Run (stdout must be the block, byte for byte, in every arm):
//   $J/javac -d out tools/probes/interp/L2/L2W92DeferredFieldSites.java
//   $B --java-home $J/.. -cp out L2W92DeferredFieldSites
//   CRATONVM_JIT_DEFERRED_FIELD_SITES=1 $B --java-home $J/.. -cp out L2W92DeferredFieldSites
//   (each also with --compatible, and once with --nojit)
//   $J/java -cp out L2W92DeferredFieldSites
// Positive control that the compiled arm was taken (switch on):
//   CRATONVM_DBG_JITC=1 CRATONVM_JIT_DEFERRED_FIELD_SITES=1 $B ... 2>&1 | grep 'deferred-field-sites L2W92DeferredFieldSites.hot'
// prints one line naming the cold sites' pcs; without the switch the same run
// prints `resolver-bail site=static_field_resolve ... L2W92DeferredFieldSites.hot`.
//
// Derived from the JDK 25 sources (JVMS §5.5, §6.5 getstatic/putstatic/
// getfield/putfield; `NullPointerException::getExtendedNPEMessage` parameter
// naming; `InstanceKlass::initialize_impl`'s "Could not initialize class"),
// not yet measured on a HotSpot build at the time of writing.
//
// HOTSPOT_EXPECTED_BEGIN
// warm 30000
// getstatic 1162
// putstatic 193
// fields 224
// putfield-npe Cannot assign field "g" because "<parameter3>" is null
// getfield-npe Cannot read field "g" because "<parameter3>" is null
// wide 305419906
// boom1 java.lang.ExceptionInInitializerError java.lang.ArithmeticException: / by zero
// boom2 java.lang.NoClassDefFoundError Could not initialize class L2W92ColdBoom
// log [clinit-ColdStatic, clinit-ColdPut, clinit-ColdWide]
// HOTSPOT_EXPECTED_END
public class L2W92DeferredFieldSites {
    static final java.util.List<String> LOG = new java.util.ArrayList<>();

    static int hot(int i, int mode, L2W92ColdNull n) {
        int acc = i * 31 + 7;
        if (mode == 1) {
            return L2W92ColdStatic.VALUE + acc;
        }
        if (mode == 2) {
            L2W92ColdPut.slot = acc;
            return 0;
        }
        if (mode == 3) {
            L2W92ColdBox b = new L2W92ColdBox();
            b.f = acc;
            return b.f;
        }
        if (mode == 4) {
            n.g = acc;
            return 0;
        }
        if (mode == 5) {
            return n.g;
        }
        if (mode == 6) {
            return (int) (L2W92ColdWide.WIDE >>> 32) + (int) (L2W92ColdWide.HALF * 4);
        }
        if (mode == 7) {
            return L2W92ColdBoom.V;
        }
        return acc;
    }

    public static void main(String[] args) {
        int sink = 0;
        int calls = 0;
        for (int i = 0; i < 30000; i++) {
            sink += hot(i, 0, null);
            calls++;
        }
        System.out.println("warm " + calls + (sink == 42 ? "!" : ""));
        System.out.println("getstatic " + hot(5, 1, null));
        hot(6, 2, null);
        System.out.println("putstatic " + L2W92ColdPut.slot);
        System.out.println("fields " + hot(7, 3, null));
        try {
            hot(8, 4, null);
            System.out.println("putfield-npe no-throw");
        } catch (NullPointerException e) {
            System.out.println("putfield-npe " + e.getMessage());
        }
        try {
            System.out.println("getfield-npe no-throw " + hot(9, 5, null));
        } catch (NullPointerException e) {
            System.out.println("getfield-npe " + e.getMessage());
        }
        System.out.println("wide " + hot(10, 6, null));
        try {
            System.out.println("boom1 no-throw " + hot(11, 7, null));
        } catch (Throwable t) {
            System.out.println("boom1 " + t.getClass().getName() + " " + t.getCause());
        }
        try {
            System.out.println("boom2 no-throw " + hot(12, 7, null));
        } catch (Throwable t) {
            System.out.println("boom2 " + t.getClass().getName() + " " + t.getMessage());
        }
        System.out.println("log " + LOG);
    }
}

class L2W92ColdStatic {
    static int VALUE;

    static {
        L2W92DeferredFieldSites.LOG.add("clinit-ColdStatic");
        VALUE = 1000;
    }
}

class L2W92ColdPut {
    static int slot;

    static {
        L2W92DeferredFieldSites.LOG.add("clinit-ColdPut");
        slot = -1;
    }
}

class L2W92ColdBox {
    int f;
}

class L2W92ColdNull {
    int g;
}

class L2W92ColdWide {
    static long WIDE;
    static double HALF;

    static {
        L2W92DeferredFieldSites.LOG.add("clinit-ColdWide");
        WIDE = 0x1234567800000000L;
        HALF = 2.5;
    }
}

class L2W92ColdBoom {
    static int V = 1 / zero();

    static int zero() {
        return 0;
    }
}
