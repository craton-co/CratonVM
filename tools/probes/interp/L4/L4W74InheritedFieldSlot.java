// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 74, lane L4 -- a defect probe (no switch), page
// `docs/internal/fixed-bugs/interpreter-i74-L4-a-finder-on-a-subclass-reads-an-inherited-instance-field-at-slot-zero-FIXED-20261111.md`.
//
// `findGetter(Derived.class, "b", int.class)` for a field `b` that `Base`
// declares: HotSpot resolves the member to its DECLARING class (`Base`) and
// the accessor reads `b`. Under `--jdk-only` (code reading) this VM's
// `MethodHandleNatives.resolve` keeps `Derived` in the member's `clazz` for an
// instance field and `objectFieldOffset` looks the name up among `Derived`'s
// OWN fields only, misses, and answers the vmindex fallback `0`: the handle
// reads (and a setter writes) slot 0, i.e. `a`. The wave-73 row
// `get-through-subclass` of `L4W73FieldNativesMemo` passes only because its
// field is the first slot. Expected to DIFF under `--jdk-only` until fixed;
// `--compatible` is the control.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// get-own: 3
// get-inherited-first: 1
// get-inherited-second: 2
// get-inherited-long: 5
// set-inherited-second: a=1 b=7
// reveal-declaring: Base
// unreflect-inherited: 2
// static-inherited: 9
// varhandle-inherited-second: 2
// set-twice-inherited-long: a=1 b=2 l=11
// HOTSPOT_EXPECTED_END
//
// Wave 79 (lane L4) fixed it: `MethodHandleNatives.resolve` writes the
// declaring class into an inherited instance field's `MemberName.clazz`, and
// `objectFieldOffset` walks the superclass chain; the last two rows were added
// then.
//
// SETUP: none; `java|cratonvm [--compatible|--jdk-only] [--nojit] -cp . L4W74InheritedFieldSlot`.
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;

public class L4W74InheritedFieldSlot {
    public static class Base {
        public static int SB = 9;
        public int a = 1;
        public int b = 2;
        public long l = 5L;
    }

    public static class Derived extends Base {
        public int c = 3;
    }

    interface Row {
        Object run() throws Throwable;
    }

    static void row(String name, Row r) {
        String out;
        try {
            out = String.valueOf(r.run());
        } catch (Throwable t) {
            out = t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(name + ": " + out);
    }

    public static void main(String[] args) {
        MethodHandles.Lookup lk = MethodHandles.lookup();
        row("get-own", () -> {
            MethodHandle h = lk.findGetter(Derived.class, "c", int.class);
            return (int) h.invokeExact(new Derived());
        });
        row("get-inherited-first", () -> {
            MethodHandle h = lk.findGetter(Derived.class, "a", int.class);
            return (int) h.invokeExact(new Derived());
        });
        row("get-inherited-second", () -> {
            MethodHandle h = lk.findGetter(Derived.class, "b", int.class);
            return (int) h.invokeExact(new Derived());
        });
        row("get-inherited-long", () -> {
            MethodHandle h = lk.findGetter(Derived.class, "l", long.class);
            return (long) h.invokeExact(new Derived());
        });
        row("set-inherited-second", () -> {
            MethodHandle h = lk.findSetter(Derived.class, "b", int.class);
            Derived d = new Derived();
            h.invokeExact(d, 7);
            return "a=" + d.a + " b=" + d.b;
        });
        row("reveal-declaring", () -> lk.revealDirect(lk.findGetter(Derived.class, "b", int.class))
                .getDeclaringClass().getSimpleName());
        row("unreflect-inherited", () -> {
            MethodHandle h = lk.unreflectGetter(Derived.class.getField("b"));
            return (int) h.invoke(new Derived());
        });
        row("static-inherited", () -> {
            MethodHandle h = lk.findStaticGetter(Derived.class, "SB", int.class);
            return (int) h.invokeExact();
        });
        row("varhandle-inherited-second", () -> {
            java.lang.invoke.VarHandle vh = lk.findVarHandle(Derived.class, "b", int.class);
            return (int) vh.get(new Derived());
        });
        row("set-twice-inherited-long", () -> {
            MethodHandle h = lk.findSetter(Derived.class, "l", long.class);
            Derived d = new Derived();
            h.invokeExact(d, 10L);
            h.invokeExact(d, 11L);
            return "a=" + d.a + " b=" + d.b + " l=" + d.l;
        });
    }
}
