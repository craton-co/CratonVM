// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// bench
//
// Interpreter round i4 wave 92, lane L3 (`CRATONVM_CLASS_INIT_SLOW_ONE_READ`,
// i72-L3-proposal-one-class-manager-read-on-the-class-init-slow-path): what
// does a re-entrant class-initialization check cost inside a running
// `<clinit>`?
//
// While a class's `<clinit>` runs, its initialized bit is clear, so every
// initialization check of the class itself (`new Self()`, `Self.count++`, a
// reflective static read of Self) takes `ensure_class_initialized_shared`'s
// slow path and answers "recursive request" (JVMS 5.5 step 2). Before the
// switch that path took a class-manager read and a name compare (the
// ConstantDescs cycle guard) BEFORE the state loop's own read; with it, one
// read. Each row defines a fresh copy of a nested class in a fresh loader and
// times `Class.forName(name, true, loader)`, i.e. its whole `<clinit>`:
//
//   clinit-new-table      2000 x `new Table(i)` (+ a static counter bump)
//   clinit-reflect-table  200 x `getDeclaredField("SEED").getInt(null)`
//   clinit-control        the same 2000-step loop with no self reference
//                         (must not move)
//
// (row - control) / steps is the per-check cost the switch should shrink.
//
// Output: "row <name> min <ns> med <ns> ns/class" and "checksum <name> <n>".
// Run: javac -d . L3W92ClinitTableBench.java
//      java -cp . L3W92ClinitTableBench                                     (HotSpot)
//      cratonvm --jdk-only -cp . L3W92ClinitTableBench
//      CRATONVM_CLASS_INIT_SLOW_ONE_READ=1 cratonvm --jdk-only -cp . L3W92ClinitTableBench
//      (and --nojit, and --compatible; interleave 5x; an optional argument
//      sets the classes per row, default 60)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// checksum clinit-new-table 1999000
// checksum clinit-reflect-table 1400
// checksum clinit-control 1999000
// HOTSPOT_EXPECTED_END
import java.io.InputStream;
import java.util.Arrays;
import java.util.Locale;

public class L3W92ClinitTableBench {
    /** `<clinit>` builds 2000 instances of itself. */
    public static final class Table {
        public static final int N = 2000;
        public static int count;
        public static final Table[] ROWS;
        public static final long SUM;
        public final int v;

        public Table(int v) {
            this.v = v;
            count++;
        }

        static {
            Table[] r = new Table[N];
            long s = 0;
            for (int i = 0; i < N; i++) {
                r[i] = new Table(i);
                s += r[i].v;
            }
            ROWS = r;
            SUM = s;
        }
    }

    /** `<clinit>` reads its own static reflectively 200 times. */
    public static final class TableRefl {
        public static int SEED = 7;
        public static final long SUM;

        static {
            long s = 0;
            try {
                for (int i = 0; i < 200; i++) {
                    s += TableRefl.class.getDeclaredField("SEED").getInt(null);
                }
            } catch (ReflectiveOperationException e) {
                throw new ExceptionInInitializerError(e);
            }
            SUM = s;
        }
    }

    /** The same 2000-step loop, no self reference. */
    public static final class Control {
        public static final long SUM;

        static {
            int[] r = new int[2000];
            long s = 0;
            for (int i = 0; i < 2000; i++) {
                r[i] = i;
                s += r[i];
            }
            SUM = s;
        }
    }

    /** Defines one class from its bytes; java.* from the boot loader. */
    static final class Loader extends ClassLoader {
        Loader() {
            super(null);
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    static byte[] bytes(String simple) throws Exception {
        String res = "L3W92ClinitTableBench$" + simple + ".class";
        try (InputStream in = L3W92ClinitTableBench.class.getResourceAsStream(res)) {
            return in.readAllBytes();
        }
    }

    static void row(String name, String simple, int count, boolean print) throws Throwable {
        byte[] b = bytes(simple);
        String binary = "L3W92ClinitTableBench$" + simple;
        long[] t = new long[count];
        long checksum = 0;
        for (int i = 0; i < count; i++) {
            Loader loader = new Loader();
            loader.define(binary, b);
            long t0 = System.nanoTime();
            Class<?> c = Class.forName(binary, true, loader);
            t[i] = System.nanoTime() - t0;
            checksum = c.getField("SUM").getLong(null);
        }
        if (print) {
            long[] s = t.clone();
            Arrays.sort(s);
            double med = s.length % 2 == 1 ? s[s.length / 2] : (s[s.length / 2 - 1] + s[s.length / 2]) / 2.0;
            System.out.printf(Locale.ROOT, "row %s min %d med %.1f ns/class%n", name, s[0], med);
            System.out.println("checksum " + name + " " + checksum);
        }
    }

    public static void main(String[] args) throws Throwable {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 60;
        // Warm-up: the reflective and loader frames past the JIT thresholds.
        for (int round = 0; round < 2; round++) {
            row("warm", "Table", 40, false);
            row("warm", "TableRefl", 40, false);
            row("warm", "Control", 40, false);
        }
        row("clinit-new-table", "Table", n, true);
        row("clinit-reflect-table", "TableRefl", n, true);
        row("clinit-control", "Control", n, true);
    }
}
