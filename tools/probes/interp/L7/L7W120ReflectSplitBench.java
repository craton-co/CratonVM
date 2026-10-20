// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// bench
//
// Interpreter round i6 wave 120, lane L7: where `Method.invoke` spends its time
// (docs/internal/fixed-bugs/interpreter-i114-L7-native-reentry-cost-split-KEPT-OFF-20261224.md, wave 120).
// The w115 fat-LTO `--nojit` numbers of L7W114NativeReentryBench against HotSpot 25 `-Xint` on
// the same machine class: plain-static 122 / 21 ns (5.8x), mh-exact-static 1371 / 167 (8.2x),
// reflect-static 31114 / 1254 (25x), reflect-virtual 44639 / 1862 (24x). The reflective rows
// are three times further from HotSpot than the method-handle row their body ends in, so the
// gap is in `Method.invoke`'s own layers, not in the `linkToStatic` re-entry. This bench splits
// those layers, one row each, every row calling the same `inc(x) = x + 1`:
//
//   mh-exact          MH_INC.invokeExact(i)                    the accessor's final step alone
//   invoke-accessible M.invoke(null, i) after setAccessible     no caller-class walk, no access check
//   invoke-checked    M.invoke(null, i), public method          + Reflection.getCallerClass + checkAccess
//   caller-class      StackWalker.getCallerClass()             the caller walk alone (its own API)
//   box-only          Integer.valueOf(i) + intValue            the boxing the accessor does
//   array-only        new Object[] {i}                         the argument array
//
// Rows are not additive exactly, but `invoke-checked - invoke-accessible` is the caller walk and
// access check, `invoke-accessible - mh-exact` the accessor's own Java (argument checks,
// unboxing, the spread), and `caller-class` prices the walk alone.
//
// Run: cratonvm --java-home <jdk25> --jdk-only --nojit -cp <dir> L7W120ReflectSplitBench
//      (and with the JIT; and HotSpot: java -Xint -cp <dir> L7W120ReflectSplitBench)
// HotSpot 25 -Xint on the lane's machine (TIME, ns/op): mh-exact 244, invoke-accessible 1605,
// invoke-checked 1334, caller-class 8394, box-only 99, array-only 124.
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// mh-exact checksum=12502500
// invoke-accessible checksum=12502500
// invoke-checked checksum=12502500
// caller-class checksum=12502500
// box-only checksum=17497500
// array-only checksum=12502500
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Method;

public class L7W120ReflectSplitBench {
    public static int inc(int x) {
        return x + 1;
    }

    static final MethodHandle MH_INC;
    static final Method M_CHECKED;
    static final Method M_OPEN;
    static final StackWalker WALKER = StackWalker.getInstance(StackWalker.Option.RETAIN_CLASS_REFERENCE);

    static {
        try {
            MH_INC = MethodHandles.lookup().findStatic(
                    L7W120ReflectSplitBench.class, "inc", MethodType.methodType(int.class, int.class));
            M_CHECKED = L7W120ReflectSplitBench.class.getMethod("inc", int.class);
            M_OPEN = L7W120ReflectSplitBench.class.getMethod("inc", int.class);
            M_OPEN.setAccessible(true);
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    interface Row {
        long run(int n) throws Throwable;
    }

    static long callerOf() {
        return WALKER.getCallerClass() == L7W120ReflectSplitBench.class ? 1 : 0;
    }

    public static void main(String[] args) throws Throwable {
        String[] names = {"mh-exact", "invoke-accessible", "invoke-checked", "caller-class", "box-only",
            "array-only"};
        Row[] rows = {
            n -> {
                long s = 0;
                for (int i = 0; i < n; i++) {
                    s += (int) MH_INC.invokeExact(i);
                }
                return s;
            },
            n -> {
                long s = 0;
                for (int i = 0; i < n; i++) {
                    s += (Integer) M_OPEN.invoke(null, i);
                }
                return s;
            },
            n -> {
                long s = 0;
                for (int i = 0; i < n; i++) {
                    s += (Integer) M_CHECKED.invoke(null, i);
                }
                return s;
            },
            n -> {
                long s = 0;
                for (int i = 0; i < n; i++) {
                    s += callerOf() + i;
                }
                return s;
            },
            n -> {
                long s = 0;
                for (int i = 0; i < n; i++) {
                    s += Integer.valueOf(i + 1000).intValue();
                }
                return s;
            },
            n -> {
                long s = 0;
                for (int i = 0; i < n; i++) {
                    Object[] a = {i};
                    s += a.length + i;
                }
                return s;
            },
        };
        final int n = 5000;
        for (int r = 0; r < rows.length; r++) {
            if (args.length > 0 && !args[0].equals(names[r])) {
                continue;
            }
            long checksum = rows[r].run(n);
            long best = Long.MAX_VALUE;
            for (int rep = 0; rep < 5; rep++) {
                long t0 = System.nanoTime();
                rows[r].run(n);
                best = Math.min(best, System.nanoTime() - t0);
            }
            System.out.println(names[r] + " checksum=" + checksum);
            System.out.println("TIME " + names[r] + " " + (best / n) + " ns/op (min of 5)");
        }
    }
}
