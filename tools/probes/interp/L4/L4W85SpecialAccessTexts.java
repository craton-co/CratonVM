// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 85, lane L4: `Lookup.findSpecial` /
// `unreflectSpecial` refusal texts (JDK 25 `MethodHandles.Lookup`:
// `checkSpecialCaller` -> `new MemberName(specialCaller).makeAccessException(
// "no private access for invokespecial", this)`, whose `MemberName(Class)`
// prints as `class X`, followed by `, from public Lookup` or
// `, from class L (module)`; `restrictReceiver` -> "caller class must be a
// subclass below the method: C.m()T/invokeSpecial, from class L (module)";
// a resolution failure -> `makeAccessException()` without a `from` tail:
// `NoSuchMethodException` for an absent method, `IllegalAccessException`
// for a static one (HotSpot's resolution raises `IncompatibleClassChangeError`
// there). An unnamed module's `toString` carries an identity hash, masked here
// as `@H`.
//
// Run: javac -d . L4W85SpecialAccessTexts.java
//      cratonvm --jdk-only [--nojit] -cp . L4W85SpecialAccessTexts
//      cratonvm --compatible [--nojit] -cp . L4W85SpecialAccessTexts
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// findSpecial-other-caller: java.lang.IllegalAccessException: no private access for invokespecial: class L4W85SpecialAccessTexts$Other, from class L4W85SpecialAccessTexts (unnamed module @H)
// findSpecial-public-lookup: java.lang.IllegalAccessException: no private access for invokespecial: class L4W85SpecialAccessTexts$Sub, from public Lookup
// findSpecial-no-private: java.lang.IllegalAccessException: no private access for invokespecial: class L4W85SpecialAccessTexts$Sub, from class L4W85SpecialAccessTexts$Sub (unnamed module @H)
// unreflectSpecial-other-caller: java.lang.IllegalAccessException: no private access for invokespecial: class L4W85SpecialAccessTexts$Other, from class L4W85SpecialAccessTexts (unnamed module @H)
// findSpecial-super-call: base
// unreflectSpecial-super-call: base
// findSpecial-unrelated: java.lang.IllegalAccessException: caller class must be a subclass below the method: L4W85SpecialAccessTexts$Unrelated.x()String/invokeSpecial, from class L4W85SpecialAccessTexts$Sub (unnamed module @H)
// findSpecial-static: java.lang.IllegalAccessException: no such method: L4W85SpecialAccessTexts$Base.s()void/invokeSpecial
// findSpecial-absent: java.lang.NoSuchMethodException: no such method: L4W85SpecialAccessTexts$Base.nope()void/invokeSpecial
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

public class L4W85SpecialAccessTexts {
    public static class Base {
        public String m() {
            return "base";
        }

        public static void s() {
        }
    }

    public static class Sub extends Base {
        @Override
        public String m() {
            return "sub";
        }

        static MethodHandles.Lookup lookup() {
            return MethodHandles.lookup();
        }
    }

    public static class Other extends Base {
    }

    public static class Unrelated {
        public String x() {
            return "x";
        }
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
        System.out.println(name + ": " + out.replaceAll("@[0-9a-f]+\\)", "@H)"));
    }

    public static void main(String[] args) {
        MethodType mt = MethodType.methodType(String.class);
        MethodType mv = MethodType.methodType(void.class);
        MethodHandles.Lookup self = MethodHandles.lookup();
        MethodHandles.Lookup sub = Sub.lookup();
        row("findSpecial-other-caller", () -> self.findSpecial(Base.class, "m", mt, Other.class));
        row("findSpecial-public-lookup",
                () -> MethodHandles.publicLookup().findSpecial(Base.class, "m", mt, Sub.class));
        row("findSpecial-no-private", () -> sub.dropLookupMode(MethodHandles.Lookup.PRIVATE)
                .findSpecial(Base.class, "m", mt, Sub.class));
        row("unreflectSpecial-other-caller",
                () -> self.unreflectSpecial(Base.class.getDeclaredMethod("m"), Other.class));
        row("findSpecial-super-call", () -> {
            MethodHandle h = sub.findSpecial(Base.class, "m", mt, Sub.class);
            return (String) h.invoke(new Sub());
        });
        row("unreflectSpecial-super-call", () -> {
            MethodHandle h = sub.unreflectSpecial(Base.class.getDeclaredMethod("m"), Sub.class);
            return (String) h.invoke(new Sub());
        });
        row("findSpecial-unrelated", () -> sub.findSpecial(Unrelated.class, "x", mt, Sub.class));
        row("findSpecial-static", () -> sub.findSpecial(Base.class, "s", mv, Sub.class));
        row("findSpecial-absent", () -> sub.findSpecial(Base.class, "nope", mv, Sub.class));
    }
}
