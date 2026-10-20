// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 117, lane L2: `CRATONVM_JIT_REENTRY_ONLY_PUBLICATION`
// (default off; page
// docs/internal/fixed-bugs/interpreter-i115-L4-proposal-shadowed-jdk-bodies-compile-for-their-bytecode-only-reentries-KEPT-OFF-20261019.md).
//
// With the switch, `MethodHandle.asType`, both `invokeWithArguments` and
// `bindTo` (JDK bodies behind a registered Intrinsic) may compile for the
// natives' bytecode-only re-entries, but their bodies publish re-entry-only:
// no compiled or interpreted caller may bind them. This probe calls the four
// hot from compiled callers (the statically bound arm for the `final`
// `asType` / `bindTo`), on ordinary direct handles AND on an FFM downcall
// handle, whose JDK `asType` body fails on a VM-minted carrier
// (`NoSuchMethodError: MethodHandle.editor()`), so a caller bound to the
// published body shows up as an exception or a wrong line. Every line must
// be the same with the switch off and on, both modes, JIT and --nojit.
//
// Run:
//   javac -d out tools/probes/interp/L2/L2W117ReentryOnlyPublication.java
//   java --enable-native-access=ALL-UNNAMED -cp out L2W117ReentryOnlyPublication          (HotSpot)
//   cratonvm --jdk-only -cp out L2W117ReentryOnlyPublication
//   CRATONVM_JIT_REENTRY_ONLY_PUBLICATION=1 cratonvm --jdk-only -cp out L2W117ReentryOnlyPublication
//   (and --compatible, and --nojit once per mode)
// Bench rows ("TIME ", ignored by the battery): the re-entered bodies run
// compiled with the switch; `TIME iwa` and `TIME as-type-new` should fall.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// as-type 100000
// as-type-new 200000
// bind-to 2000000
// iwa 1500000
// iwa-list 1500000
// downcall-as-type 1000
// downcall-iwa 1000
// HOTSPOT_EXPECTED_END
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.Linker;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.List;

public class L2W117ReentryOnlyPublication {
    static final int CALLS = 100_000;

    static int add(int a, int b) { return a + b; }
    static int scale(Integer k, int a) { return k * a; }

    static final MethodHandle ADD;
    static final MethodHandle SCALE;
    static final MethodType OBJ2 = MethodType.methodType(Object.class, Object.class, Object.class);
    static final MethodType LONG2 = MethodType.methodType(long.class, int.class, int.class);
    static {
        try {
            MethodHandles.Lookup l = MethodHandles.lookup();
            ADD = l.findStatic(L2W117ReentryOnlyPublication.class, "add",
                    MethodType.methodType(int.class, int.class, int.class));
            SCALE = l.findStatic(L2W117ReentryOnlyPublication.class, "scale",
                    MethodType.methodType(int.class, Integer.class, int.class));
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    static long asType() throws Throwable {
        long sum = 0;
        for (int i = 0; i < CALLS; i++) {
            MethodHandle h = ADD.asType(OBJ2);
            sum += (int) (Object) h.invokeExact((Object) Integer.valueOf(i & 7), (Object) Integer.valueOf(3)) - 3;
        }
        return sum / 7 * 2;
    }

    static long asTypeNew() throws Throwable {
        long sum = 0;
        for (int i = 0; i < CALLS; i++) {
            // A type the cache does not hold: the JDK body's slow path.
            MethodHandle h = ADD.asType(((i & 1) == 0) ? LONG2 : OBJ2);
            sum += 2;
            if (h.type().returnType() == void.class) {
                throw new AssertionError();
            }
        }
        return sum;
    }

    static long bindTo() throws Throwable {
        long sum = 0;
        for (int i = 0; i < CALLS; i++) {
            MethodHandle h = SCALE.bindTo(Integer.valueOf(2));
            sum += (int) h.invokeExact(10) / 1;
        }
        return sum;
    }

    static long iwa() throws Throwable {
        long sum = 0;
        for (int i = 0; i < CALLS; i++) {
            sum += (int) ADD.invokeWithArguments(i & 7, 12) - (i & 7);
            sum += 3;
        }
        return sum;
    }

    static long iwaList() throws Throwable {
        long sum = 0;
        List<Object> args = List.of(4, 11);
        for (int i = 0; i < CALLS; i++) {
            sum += (int) ADD.invokeWithArguments(args);
        }
        return sum;
    }

    static void time(String row, java.util.concurrent.Callable<Long> body) throws Exception {
        long t0 = System.nanoTime();
        long r = body.call();
        long t = System.nanoTime() - t0;
        System.out.println("TIME " + row + " " + t / CALLS + " ns/call");
        System.out.println(row + " " + r);
    }

    interface Body { long run() throws Throwable; }

    static java.util.concurrent.Callable<Long> c(Body b) {
        return () -> {
            try {
                return b.run();
            } catch (Throwable e) {
                throw new RuntimeException(e);
            }
        };
    }

    public static void main(String[] args) throws Throwable {
        for (int w = 0; w < 2; w++) {
            asType(); bindTo(); iwa();
        }
        time("as-type", c(L2W117ReentryOnlyPublication::asType));
        time("as-type-new", c(L2W117ReentryOnlyPublication::asTypeNew));
        time("bind-to", c(L2W117ReentryOnlyPublication::bindTo));
        time("iwa", c(L2W117ReentryOnlyPublication::iwa));
        time("iwa-list", c(L2W117ReentryOnlyPublication::iwaList));
        // A downcall handle: the carrier the JDK bodies must never run on.
        Linker linker = Linker.nativeLinker();
        MethodHandle labs = linker.defaultLookup().find("labs")
                .or(() -> linker.defaultLookup().find("abs"))
                .map(seg -> linker.downcallHandle(seg,
                        FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.JAVA_INT)))
                .orElse(null);
        if (labs == null) {
            System.out.println("downcall-as-type absent");
            System.out.println("downcall-iwa absent");
            return;
        }
        long a = 0;
        long b = 0;
        MethodType generic = MethodType.methodType(Object.class, Object.class);
        try {
            for (int i = 0; i < 1000; i++) {
                MethodHandle g = labs.asType(generic);
                a += (int) (Object) g.invokeExact((Object) Integer.valueOf(-1));
            }
            System.out.println("downcall-as-type " + a);
        } catch (Throwable e) {
            System.out.println("downcall-as-type " + e.getClass().getName() + ": " + e.getMessage());
        }
        try {
            for (int i = 0; i < 1000; i++) {
                b += (int) labs.invokeWithArguments(-1);
            }
            System.out.println("downcall-iwa " + b);
        } catch (Throwable e) {
            System.out.println("downcall-iwa " + e.getClass().getName() + ": " + e.getMessage());
        }
    }
}
