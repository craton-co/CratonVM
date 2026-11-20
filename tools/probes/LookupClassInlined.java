// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company
//
// `MethodHandles.lookup().lookupClass()` inside a small static method must name
// that method's own class however the JIT compiles its caller. A wrong answer is
// a Lookup with another class's access rights.
//
// The shape that failed: both loops run in `main`, so `main` is OSR-compiled with
// `look()` inlined while `lookup()` runs as its own compiled activation. The
// caller-class walk dropped `main`'s OSR half and the `look` level it had inlined,
// and answered `main`'s class. A denied `Method.invoke` first is what steered the
// compile into that shape on the measured binaries.
//
// Rows (HotSpot 25 prints `wrong=0` on both and `denied=300001`):
//   look   -- 300 000 x `Friend.look()` / `Stranger.look()`;
//   invoke -- the same interleaved with a Method.invoke of a private method,
//             allowed for the nestmate `Friend`, denied for the top-level `Stranger`.
//
// Unit test: vm stackwalker::tail_layout_tests::an_osr_continuations_inlined_levels_stay_in_the_class_id_walk
// Run:  javac -d out LookupClassInlined.java
//       cratonvm --compatible -cp out LookupClassInlined   (and --real-jdk);
//       the output must equal `java -cp out LookupClassInlined`.

import java.lang.invoke.MethodHandles;
import java.lang.reflect.Method;

public class LookupClassInlined {
    static class Target {
        private int secret(int x) { return x + 1; }
    }

    static class Friend {
        static int call(Method m, Target t, int i) throws Exception { return (Integer) m.invoke(t, i); }

        static Class<?> look() { return MethodHandles.lookup().lookupClass(); }
    }

    public static void main(String[] a) throws Exception {
        Method m = Target.class.getDeclaredMethod("secret", int.class);
        Target t = new Target();
        long ok = 0;
        int denied = 0;
        try {
            ok += Stranger.call(m, t, 0);
        } catch (IllegalAccessException e) {
            denied++;
        }
        int wrong = 0;
        for (int i = 0; i < 300_000; i++) {
            if (Friend.look() != Friend.class) wrong++;
            if (Stranger.look() != Stranger.class) wrong++;
        }
        System.out.println("look wrong=" + wrong);
        wrong = 0;
        for (int i = 0; i < 300_000; i++) {
            try {
                ok += Friend.call(m, t, i);
            } catch (IllegalAccessException e) {
                denied++;
            }
            try {
                ok += Stranger.call(m, t, i);
            } catch (IllegalAccessException e) {
                denied++;
            }
            if (Friend.look() != Friend.class) wrong++;
            if (Stranger.look() != Stranger.class) wrong++;
        }
        System.out.println("invoke wrong=" + wrong + " ok=" + ok + " denied=" + denied);
    }
}

// Top level: no access to `LookupClassInlined.Target`'s private method.
class Stranger {
    static int call(Method m, Object t, int i) throws Exception { return (Integer) m.invoke(t, i); }

    static Class<?> look() { return MethodHandles.lookup().lookupClass(); }
}
