// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// Prices the two things Mockito's Java9PlusLocationImpl does per mock invocation, in isolation:
// a reflective Method.invoke, and a StackWalker walk. Run under the default mode and --jdk-only.
// See docs/internal/jdk-only/heap-bytebuffer-and-chm-run-interpreted-under-jdk-only-20260918.md (retired)
import java.lang.reflect.Method;
import java.util.stream.Collectors;

public class ReflectionStackPerf {
    static long sink;
    interface Body { void run(int n); }

    static void time(String name, int n, Body b) {
        long best = Long.MAX_VALUE;
        for (int r = 0; r < 4; r++) {
            long t = System.nanoTime();
            b.run(n);
            best = Math.min(best, System.nanoTime() - t);
        }
        System.out.printf("%-40s %10.1f ns/call%n", name, best / (double) n);
    }

    static int deep(int d, Runnable r) { if (d == 0) { r.run(); return 0; } return 1 + deep(d - 1, r); }

    public static void main(String[] a) throws Exception {
        int n = a.length > 0 ? Integer.parseInt(a[0]) : 100_000;
        Method len = String.class.getMethod("length");
        Method name = Method.class.getMethod("getName");
        String s = "hello";
        time("Method.invoke String.length()", n, k -> { long x = 0; try { for (int i = 0; i < k; i++) x += (Integer) len.invoke(s); } catch (Exception e) { throw new RuntimeException(e); } sink += x; });
        time("Method.invoke Method.getName()", n, k -> { long x = 0; try { for (int i = 0; i < k; i++) x += ((String) name.invoke(len)).length(); } catch (Exception e) { throw new RuntimeException(e); } sink += x; });
        StackWalker w = StackWalker.getInstance();
        time("StackWalker.walk depth~8 (first only)", n / 10, k -> { long x = 0; for (int i = 0; i < k; i++) x += w.walk(st -> st.findFirst().get().getMethodName().length()); sink += x; });
        time("StackWalker.walk depth~8 (collect all)", n / 10, k -> { long x = 0; for (int i = 0; i < k; i++) x += w.walk(st -> st.collect(Collectors.toList())).size(); sink += x; });
        time("StackWalker.walk depth~60 (collect all)", n / 100, k -> { long x = 0; for (int i = 0; i < k; i++) x += deep(50, () -> sink += w.walk(st -> st.collect(Collectors.toList())).size()); sink += x; });
        time("new Throwable().getStackTrace() depth~8", n / 10, k -> { long x = 0; for (int i = 0; i < k; i++) x += new Throwable().getStackTrace().length; sink += x; });
        System.out.println("sink=" + sink);
    }
}
