// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// bench
//
// Interpreter round i5 wave 109, lane L2: bench and behaviour probe for
// statically bound method handles on the compiled paths
// (`CRATONVM_JIT_MH_COMPILED_LINK_SPECIAL`, default off; wave 108's
// `CRATONVM_MHN_RESOLVE_DIRECT_CALL`, default on, is the other axis).
//
// With CRATONVM_MHN_RESOLVE_DIRECT_CALL on, `MethodHandleNatives.resolve`
// settles a final / private / final-class virtual as REF_invokeSpecial (as
// HotSpot), so its LambdaForm is `DirectMethodHandle$Holder.invokeSpecial`,
// which ends in `MethodHandle.linkToSpecial`. The JIT's compiled-link redirect
// (`vm/src/jit/helpers.rs` `mh_compiled_link_redirect`) linked only
// linkToStatic / linkToVirtual / linkToInterface, so a compiled
// `invokeSpecial` form crossed the native door per call. With the new switch
// on, `linkToSpecial` calls its member as a compiled `invokespecial`, and a
// constant `invokeExact` site binds a `$Special` handle (or a plain one over a
// private method) to that call.
//
// Behaviour lines (both modes, every arm, JIT and --nojit, must equal
// HotSpot): each handle's answer, the null-receiver exception, a findSpecial
// of a superclass method from a subclass (must run the SUPER body, not the
// override), a constructor handle, and the checksums.
//
// Timing lines start with "TIME " (filter them before diffing):
//   TIME <row> med <ns> ns/op      (median of reps, 20000 calls per rep)
//
// Run: javac -d . L2W109FinalHandleBench.java
//      java -cp . L2W109FinalHandleBench                                      (HotSpot)
//      CRATONVM_MHN_RESOLVE_DIRECT_CALL=0 cratonvm --jdk-only -cp . L2W109FinalHandleBench 7   (reference arm A)
//      cratonvm --jdk-only -cp . L2W109FinalHandleBench 7                                       (arm B: default)
//      CRATONVM_JIT_MH_COMPILED_LINK_SPECIAL=1 cratonvm --jdk-only -cp . L2W109FinalHandleBench 7 (arm C)
//      (interleave A/B/C, take medians; repeat with --compatible and --nojit for the behaviour lines)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25; derived from the sources; TIME lines removed)
// final-class 5
// final-method 3
// private-special 11
// private-unreflect 13
// plain-virtual 7 70
// super-special 7
// ctor 9
// checksum final-class-length 100000
// checksum final-method 60000
// checksum private-findspecial 220000
// checksum private-unreflect 260000
// checksum plain-virtual 140000
// checksum super-findspecial 140000
// checksum ctor 180000
// null-final-class java.lang.NullPointerException
// null-final-method java.lang.NullPointerException
// null-private-special java.lang.NullPointerException
// null-private-unreflect java.lang.NullPointerException
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.Arrays;
import java.util.Locale;

public class L2W109FinalHandleBench {
    public static class Base {
        public int v = 7;

        public int get() {
            return v;
        }

        public final int fin() {
            return 3;
        }
    }

    public static class Sub extends Base {
        @Override
        public int get() {
            return 70;
        }

        /** findSpecial(Base.get) with Sub as the special caller: `super.get()`. */
        static MethodHandle superGet() throws Throwable {
            return MethodHandles.lookup().findSpecial(Base.class, "get",
                    MethodType.methodType(int.class), Sub.class);
        }
    }

    public static final class Pt {
        public final int v;

        public Pt(int v) {
            this.v = v;
        }
    }

    private int priv() {
        return 11;
    }

    private int priv2() {
        return 13;
    }

    static final MethodHandle LEN;
    static final MethodHandle FIN;
    static final MethodHandle PRIV;
    static final MethodHandle PRIV2;
    static final MethodHandle GET;
    static final MethodHandle SUPER_GET;
    static final MethodHandle CTOR;

    static {
        try {
            MethodHandles.Lookup l = MethodHandles.lookup();
            MethodType i = MethodType.methodType(int.class);
            LEN = l.findVirtual(String.class, "length", i);
            FIN = l.findVirtual(Base.class, "fin", i);
            PRIV = l.findSpecial(L2W109FinalHandleBench.class, "priv", i, L2W109FinalHandleBench.class);
            PRIV2 = l.unreflect(L2W109FinalHandleBench.class.getDeclaredMethod("priv2"));
            GET = l.findVirtual(Base.class, "get", i);
            SUPER_GET = Sub.superGet();
            CTOR = l.findConstructor(Pt.class, MethodType.methodType(void.class, int.class));
        } catch (Throwable t) {
            throw new ExceptionInInitializerError(t);
        }
    }

    static double median(long[] xs) {
        long[] c = xs.clone();
        Arrays.sort(c);
        int m = c.length / 2;
        return c.length % 2 == 1 ? c[m] : (c[m - 1] + c[m]) / 2.0;
    }

    static final int CALLS = 20000;

    interface Row {
        long run() throws Throwable;
    }

    static void bench(String row, int reps, Row r) throws Throwable {
        long[] t = new long[reps];
        long sum0 = 0;
        for (int i = 0; i < reps; i++) {
            long t0 = System.nanoTime();
            long sum = r.run();
            t[i] = (System.nanoTime() - t0) / CALLS;
            if (i == 0) {
                sum0 = sum;
            }
        }
        System.out.printf(Locale.ROOT, "TIME %s med %.1f ns/op%n", row, median(t));
        System.out.println("checksum " + row + " " + sum0);
    }

    interface NullCall {
        int call() throws Throwable;
    }

    static void nullRow(String row, NullCall c) {
        // Warm first, so the null meets the compiled site.
        String seen = "no exception";
        for (int i = 0; i < 3; i++) {
            try {
                c.call();
            } catch (Throwable e) {
                seen = e.getClass().getName();
            }
        }
        System.out.println(row + " " + seen);
    }

    public static void main(String[] args) throws Throwable {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 5;
        L2W109FinalHandleBench self = new L2W109FinalHandleBench();
        Base base = new Base();
        Base sub = new Sub();
        Sub subExact = new Sub();

        for (int i = 0; i < 300; i++) {
            int a = (int) LEN.invokeExact("hello");
            int b = (int) FIN.invokeExact(base);
            int c = (int) PRIV.invokeExact(self);
            int d = (int) PRIV2.invokeExact(self);
            int e = (int) GET.invokeExact(base);
            int f = (int) SUPER_GET.invokeExact(subExact);
            Pt g = (Pt) CTOR.invokeExact(9);
        }
        System.out.println("final-class " + (int) LEN.invokeExact("hello"));
        System.out.println("final-method " + (int) FIN.invokeExact(sub));
        System.out.println("private-special " + (int) PRIV.invokeExact(self));
        System.out.println("private-unreflect " + (int) PRIV2.invokeExact(self));
        System.out.println("plain-virtual " + (int) GET.invokeExact(base) + " " + (int) GET.invokeExact(sub));
        System.out.println("super-special " + (int) SUPER_GET.invokeExact(subExact));
        System.out.println("ctor " + ((Pt) CTOR.invokeExact(9)).v);

        bench("final-class-length", reps, () -> {
            long sum = 0;
            for (int i = 0; i < CALLS; i++) {
                sum += (int) LEN.invokeExact("hello");
            }
            return sum;
        });
        bench("final-method", reps, () -> {
            long sum = 0;
            for (int i = 0; i < CALLS; i++) {
                sum += (int) FIN.invokeExact(base);
            }
            return sum;
        });
        bench("private-findspecial", reps, () -> {
            long sum = 0;
            for (int i = 0; i < CALLS; i++) {
                sum += (int) PRIV.invokeExact(self);
            }
            return sum;
        });
        bench("private-unreflect", reps, () -> {
            long sum = 0;
            for (int i = 0; i < CALLS; i++) {
                sum += (int) PRIV2.invokeExact(self);
            }
            return sum;
        });
        bench("plain-virtual", reps, () -> {
            long sum = 0;
            for (int i = 0; i < CALLS; i++) {
                sum += (int) GET.invokeExact(base);
            }
            return sum;
        });
        bench("super-findspecial", reps, () -> {
            long sum = 0;
            for (int i = 0; i < CALLS; i++) {
                sum += (int) SUPER_GET.invokeExact(subExact);
            }
            return sum;
        });
        bench("ctor", reps, () -> {
            long sum = 0;
            for (int i = 0; i < CALLS; i++) {
                sum += ((Pt) CTOR.invokeExact(9)).v;
            }
            return sum;
        });

        nullRow("null-final-class", () -> (int) LEN.invokeExact((String) null));
        nullRow("null-final-method", () -> (int) FIN.invokeExact((Base) null));
        nullRow("null-private-special", () -> (int) PRIV.invokeExact((L2W109FinalHandleBench) null));
        nullRow("null-private-unreflect", () -> (int) PRIV2.invokeExact((L2W109FinalHandleBench) null));
    }
}
