// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 103, lane L4: behaviour probe (and bench, TIME
// lines) for `CRATONVM_JIT_MH_COMPILED_LINK_USER_SITE` (default off; page
// `docs/internal/fixed-bugs/interpreter-i95-L4-a-warm-field-handle-call-runs-the-jdk-linker-per-call-FIXED-20261018.md`,
// "Wave 103").
//
// With the switch on, a COMPILED `invokeExact` site whose descriptor names an
// application class (`(LBox;)I`) is bound to the JDK invoker linker from the
// door's basic-member row and the caller's site-type row, so it calls the
// linker form directly (as HotSpot's `invokehandle`) instead of the door.
// Every answer and refusal must be the same in both arms, both modes, JIT and
// --nojit. Each `hot*` method runs 30000 times first so the JIT compiles it.
//
// Rows:
//   getter / setter / virtual / static       values through a warm compiled site
//   other-handle                             a second handle of the same type at the same site
//   null-receiver                            NPE (`checkBase` / the door's NPE)
//   wrong-type                               a handle of another type at the site:
//                                            WrongMethodTypeException from `checkExactType`
//   target-throws                            the target's exception and the visible frames
//                                            of its trace (hidden frames filtered, as HotSpot)
//   iface                                    a `DirectMethodHandle$Interface` handle
//                                            (wave 104: the switch's remaining target)
//   bound                                    CONTROL: `bindTo` gives `()I`, a shareable
//                                            site the switch does not touch
//
// Run: javac -d . L4W103CompiledUserSiteLink.java
//      java -cp . L4W103CompiledUserSiteLink                                        (HotSpot)
//      cratonvm --jdk-only -cp . L4W103CompiledUserSiteLink
//      CRATONVM_JIT_MH_COMPILED_LINK_USER_SITE=1 cratonvm --jdk-only -cp . L4W103CompiledUserSiteLink
//      (both modes; also --nojit, where the switch cannot act)
// Timings: "TIME <row> med <ns> ns/op", printed only with the `time` argument
// (wave 105; args [time] [reps], reps default 5 with `time`, else 1), so a
// plain run prints exactly HotSpot's lines. A/B: pass `time`.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25; derived from the sources)
// getter 210000
// setter 29999
// virtual 240000
// static 90000
// iface 330000
// other-handle 11
// null-receiver java.lang.NullPointerException
// wrong-type java.lang.invoke.WrongMethodTypeException
// target-throws java.lang.IllegalStateException boom
// target-frame L4W103CompiledUserSiteLink$Box.fail
// target-frame L4W103CompiledUserSiteLink.callFail
// target-frame L4W103CompiledUserSiteLink.main
// bound 210000
// checksum bench-getter 1400000
// checksum bench-iface 2200000
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.Arrays;
import java.util.Locale;

public class L4W103CompiledUserSiteLink {
    public interface Valued {
        int value();
    }

    public static class Box implements Valued {
        public int v = 7;
        public int w = 11;

        public int plusOne() {
            return v + 1;
        }

        public int value() {
            return w;
        }

        public int fail() {
            throw new IllegalStateException("boom");
        }

        public static int three(Box b) {
            return 3;
        }
    }

    static final int WARM = 30000;

    /** Wave 105: print TIME lines (the `time` argument). */
    static boolean timing;

    static long hotGetter(MethodHandle mh, Box box, int n) throws Throwable {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            acc += (int) mh.invokeExact(box);
        }
        return acc;
    }

    static long hotIface(MethodHandle mh, Valued x, int n) throws Throwable {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            acc += (int) mh.invokeExact(x);
        }
        return acc;
    }

    static void hotSetter(MethodHandle mh, Box box, int n) throws Throwable {
        for (int i = 0; i < n; i++) {
            mh.invokeExact(box, i);
        }
    }

    static long hotBound(MethodHandle mh, int n) throws Throwable {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            acc += (int) mh.invokeExact();
        }
        return acc;
    }

    static int callFail(MethodHandle mh, Box box) throws Throwable {
        return (int) mh.invokeExact(box);
    }

    static double median(long[] xs) {
        long[] c = xs.clone();
        Arrays.sort(c);
        int m = c.length / 2;
        return c.length % 2 == 1 ? c[m] : (c[m - 1] + c[m]) / 2.0;
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        MethodHandle getter = lookup.findGetter(Box.class, "v", int.class);
        MethodHandle getterW = lookup.findGetter(Box.class, "w", int.class);
        MethodHandle setter = lookup.findSetter(Box.class, "v", int.class);
        MethodHandle virtual = lookup.findVirtual(Box.class, "plusOne", MethodType.methodType(int.class));
        MethodHandle stat = lookup.findStatic(Box.class, "three", MethodType.methodType(int.class, Box.class));
        MethodHandle fail = lookup.findVirtual(Box.class, "fail", MethodType.methodType(int.class));
        MethodHandle iface = lookup.findVirtual(Valued.class, "value", MethodType.methodType(int.class));
        MethodHandle voidGetter = MethodHandles.dropReturn(getter);
        Box box = new Box();

        System.out.println("getter " + hotGetter(getter, box, WARM));
        Box sbox = new Box();
        hotSetter(setter, sbox, WARM);
        System.out.println("setter " + sbox.v);
        System.out.println("virtual " + hotGetter(virtual, box, WARM));
        System.out.println("static " + hotGetter(stat, box, WARM));
        System.out.println("iface " + hotIface(iface, box, WARM));
        System.out.println("other-handle " + hotGetter(getterW, box, 1));
        try {
            System.out.println("null-receiver no-throw " + hotGetter(getter, null, 1));
        } catch (Throwable t) {
            System.out.println("null-receiver " + t.getClass().getName());
        }
        try {
            // `(Box)void` at the `(Box)int` site of the compiled method.
            System.out.println("wrong-type no-throw " + hotGetter(voidGetter, box, 1));
        } catch (Throwable t) {
            System.out.println("wrong-type " + t.getClass().getName());
        }
        // Warm the throwing site through the same compiled method shape first.
        for (int i = 0; i < WARM; i++) {
            callFail(virtual, box);
        }
        try {
            System.out.println("target-throws no-throw " + callFail(fail, box));
        } catch (Throwable t) {
            System.out.println("target-throws " + t.getClass().getName() + " " + t.getMessage());
            for (StackTraceElement e : t.getStackTrace()) {
                String cls = e.getClassName();
                if (cls.startsWith("L4W103CompiledUserSiteLink")) {
                    System.out.println("target-frame " + cls + "." + e.getMethodName());
                }
            }
        }
        MethodHandle bound = getter.bindTo(box);
        System.out.println("bound " + hotBound(bound, WARM));

        // Wave 105: timings only with the `time` argument, so the default run
        // prints exactly HotSpot's lines (the battery compares raw output).
        int reps = -1;
        for (String a : args) {
            if (a.equals("time")) {
                timing = true;
            } else {
                reps = Integer.parseInt(a);
            }
        }
        if (reps < 0) {
            reps = timing ? 5 : 1;
        }
        int n = 200000;
        long[] t = new long[reps];
        long sum0 = 0;
        for (int r = 0; r < reps; r++) {
            long t0 = System.nanoTime();
            long s = hotGetter(getter, box, n);
            t[r] = (System.nanoTime() - t0) / n;
            if (r == 0) {
                sum0 = s;
            }
        }
        if (timing) System.out.printf(Locale.ROOT, "TIME bench-getter med %.1f ns/op%n", median(t));
        System.out.println("checksum bench-getter " + sum0);
        // Wave 104: a handle no before-linker lane serves (`DirectMethodHandle$Interface`
        // at a `(LValued;)I` site), the only kind the switch still binds to the
        // compiled linker chain; the getter row above is now the door's in both arms.
        for (int r = 0; r < reps; r++) {
            long t0 = System.nanoTime();
            long s = hotIface(iface, box, n);
            t[r] = (System.nanoTime() - t0) / n;
            if (r == 0) {
                sum0 = s;
            }
        }
        if (timing) System.out.printf(Locale.ROOT, "TIME bench-iface med %.1f ns/op%n", median(t));
        System.out.println("checksum bench-iface " + sum0);
    }
}
