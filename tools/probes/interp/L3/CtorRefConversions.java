// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1, wave 17, lane L4: a constructor reference
// (`REF_newInvokeSpecial`, emitted directly by javac for a static nested or
// top-level class) gets the same SAM-to-implementation conversions as a
// method reference:
//
//   box   - `Function<Integer, Box> f = Box::new` over `Box(int)`: the boxed
//           argument is unboxed for the `int` parameter;
//   cast  - `Function<String, Named>` called raw with an Integer: the
//           instantiated-type checkcast raises ClassCastException;
//   void  - `Consumer<String> c = Counter::new`: the new object is discarded
//           (a void SAM), 1000 times.
//
// Before wave 17 the interpreter's constructor-reference arm skipped the
// conversions (the boxed Integer reached the `int` slot, the cast was not
// made) and answered the object to a void SAM.
//
// Run with the default settings and with --nojit; HotSpot 25 prints exactly:
//
//   box=41
//   CCE
//   made=1000

import java.util.function.Consumer;
import java.util.function.Function;

public class CtorRefConversions {
    static class Box {
        final int v;

        Box(int v) {
            this.v = v;
        }
    }

    static class Named {
        final String s;

        Named(String s) {
            this.s = s;
        }
    }

    static class Counter {
        static int made;

        Counter(String s) {
            made++;
        }
    }

    @SuppressWarnings({"rawtypes", "unchecked"})
    public static void main(String[] a) {
        Function<Integer, Box> f = Box::new;
        System.out.println("box=" + f.apply(41).v);

        Function<String, Named> g = Named::new;
        Function raw = g;
        try {
            raw.apply(Integer.valueOf(42));
            System.out.println("no CCE");
        } catch (ClassCastException e) {
            System.out.println("CCE");
        }

        Consumer<String> c = Counter::new;
        for (int i = 0; i < 1000; i++) {
            c.accept("x");
        }
        System.out.println("made=" + Counter.made);
    }
}
