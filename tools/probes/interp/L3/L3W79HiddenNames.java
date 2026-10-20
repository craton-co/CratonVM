// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 79, lane L3 -- a hidden class's `/0x<hex>` tail
// in two places it used to be misread:
//
//   * `could-not-init`: a hidden class whose <clinit> threw is used again;
//     HotSpot's NoClassDefFoundError reads "Could not initialize class
//     <Class.getName()>", the `/0x` tail kept
//     (docs/internal/fixed-bugs/interpreter-i71-L3-could-not-initialize-a-hidden-class-dots-its-0x-suffix-FIXED-20261111.md);
//   * `pkg-override`: `Method.invoke` of a package-private method on a
//     receiver of a HIDDEN subclass defined in the same package runs the
//     subclass's override (a hidden class is in its lookup class's runtime
//     package), not the declaring class's body
//     (docs/internal/fixed-bugs/interpreter-i77-L3-method-invoke-crosses-package-reads-a-hidden-class-suffix-as-a-package-FIXED-20261111.md).
//
// The hex is normalised to `/0xN`. Run --compatible and --jdk-only, default
// and --nojit.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// hidden-name L3W79HiddenBad/0xN
// first java.lang.ExceptionInInitializerError cause=java.lang.IllegalStateException: boom
// second java.lang.NoClassDefFoundError: Could not initialize class L3W79HiddenBad/0xN
// second-cause java.lang.ExceptionInInitializerError: Exception java.lang.IllegalStateException: boom [in thread "main"]
// sub-name L3W79HiddenSub/0xN
// pkg-base base
// pkg-override sub
// pkg-direct sub
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandles;
import java.lang.reflect.Method;

public class L3W79HiddenNames {
    static class Base {
        String m() {
            return "base";
        }
    }

    static String norm(String s) {
        return s == null ? null : s.replaceAll("/0x[0-9a-fA-F]+", "/0xN");
    }

    static byte[] bytes(String resource) throws Exception {
        try (var in = L3W79HiddenNames.class.getResourceAsStream(resource)) {
            return in.readAllBytes();
        }
    }

    public static void main(String[] args) throws Exception {
        MethodHandles.Lookup bad =
                MethodHandles.lookup().defineHiddenClass(bytes("L3W79HiddenBad.class"), false);
        System.out.println("hidden-name " + norm(bad.lookupClass().getName()));
        try {
            bad.ensureInitialized(bad.lookupClass());
            System.out.println("first initialized");
        } catch (Throwable t) {
            Throwable c = t.getCause();
            System.out.println("first " + t.getClass().getName() + " cause="
                    + (c == null ? "null" : c.getClass().getName() + ": " + c.getMessage()));
        }
        try {
            bad.ensureInitialized(bad.lookupClass());
            System.out.println("second initialized");
        } catch (Throwable t) {
            System.out.println("second " + t.getClass().getName() + ": " + norm(t.getMessage()));
            Throwable c = t.getCause();
            System.out.println("second-cause "
                    + (c == null ? "null" : c.getClass().getName() + ": " + norm(c.getMessage())));
        }

        MethodHandles.Lookup sub =
                MethodHandles.lookup().defineHiddenClass(bytes("L3W79HiddenSub.class"), true);
        Class<?> subClass = sub.lookupClass();
        System.out.println("sub-name " + norm(subClass.getName()));
        Object hiddenSub = subClass.getDeclaredConstructor().newInstance();
        Method m = Base.class.getDeclaredMethod("m");
        System.out.println("pkg-base " + m.invoke(new Base()));
        System.out.println("pkg-override " + m.invoke(hiddenSub));
        System.out.println("pkg-direct " + ((Base) hiddenSub).m());
    }
}

// The bytes of a hidden class whose <clinit> throws: a top-level class, so its
// class file has no InnerClasses entry naming itself.
class L3W79HiddenBad {
    static final int VALUE = fail();

    static int fail() {
        throw new IllegalStateException("boom");
    }
}

// The bytes of a hidden subclass that overrides the package-private `m()`.
class L3W79HiddenSub extends L3W79HiddenNames.Base {
    public L3W79HiddenSub() {
    }

    @Override
    String m() {
        return "sub";
    }
}
