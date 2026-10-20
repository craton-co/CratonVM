// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 47, lane L4: the rows of proposal
// `interpreter-L4-proposal-one-varhandle-row-read-per-access-FIXED-20261011` (`VARHANDLE_ONE_ROW_READ`,
// `native-builtins/src/lang_invoke.rs`). Run under `--nojit`, where every
// access reaches the access-mode natives: the switch-on build reads the
// handle's side-table row once per access (it was read three to six times),
// skips the two FFM predicates for an instance or static row, and a static
// `get` no longer takes the null-coordinate slow path. Rows (ns/op on stderr):
//
//   get-int          `(int) I.get(h)`                  instance, int
//   set-int          `I.set(h, i)`                     instance, int
//   cas-int          `I.compareAndSet(h, a, b)`        instance, int
//   get-ref          `(Object) R.get(h)`               instance, reference
//   static-get-int   `(int) S.get()`                   static, int (no coordinate)
//   static-set-null  `SR.set((Object) null)`           static, reference, null value
//   getandadd-int    `(int) I.getAndAdd(h, 1)`         instance, int
//   getandset-ref    `(Object) R.getAndSet(h, o)`      instance, reference
//   cae-int          `(int) I.compareAndExchange(h, a, b)`
//
// How to run: `cratonvm --java-home <jdk25> --nojit -cp <dir>
// L4W47VarHandleNativeBench`, the switch-on build interleaved against the
// same build with the last lane commit reverted, fat LTO, medians of several
// runs (in-VM timings swing ~3x between runs on the Windows host).
//
// Stdout is a deterministic checksum per row and must equal HotSpot 25's
// (`-Xint` too). HotSpot 25 (25.0.3) prints:
//   get-int 1249975000
//   set-int 1249975000
//   cas-int 50000
//   get-ref 50000
//   static-get-int 1249975000
//   static-set-null 0
//   getandadd-int 1249975000
//   getandset-ref 50000
//   cae-int 1249975000
import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;

public class L4W47VarHandleNativeBench {
    static final int WARMUP = 5_000;
    static final int ITERS = 50_000;

    static final class H {
        int i;
        Object r = "x";
    }

    static int si;
    static Object sr;

    static final VarHandle I;
    static final VarHandle R;
    static final VarHandle S;
    static final VarHandle SR;

    static {
        try {
            MethodHandles.Lookup l = MethodHandles.lookup();
            I = l.findVarHandle(H.class, "i", int.class);
            R = l.findVarHandle(H.class, "r", Object.class);
            S = l.findStaticVarHandle(L4W47VarHandleNativeBench.class, "si", int.class);
            SR = l.findStaticVarHandle(L4W47VarHandleNativeBench.class, "sr", Object.class);
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    interface Row {
        long run(H h, int n);
    }

    static long getInt(H h, int n) {
        long sum = 0;
        for (int k = 0; k < n; k++) {
            h.i = k;
            sum += (int) I.get(h);
        }
        return sum;
    }

    static long setInt(H h, int n) {
        long sum = 0;
        for (int k = 0; k < n; k++) {
            I.set(h, k);
            sum += h.i;
        }
        return sum;
    }

    static long casInt(H h, int n) {
        long ok = 0;
        h.i = 0;
        for (int k = 0; k < n; k++) {
            if (I.compareAndSet(h, k, k + 1)) {
                ok++;
            }
        }
        return ok;
    }

    static long getRef(H h, int n) {
        long seen = 0;
        for (int k = 0; k < n; k++) {
            if ((Object) R.get(h) != null) {
                seen++;
            }
        }
        return seen;
    }

    static long staticGetInt(H h, int n) {
        long sum = 0;
        for (int k = 0; k < n; k++) {
            si = k;
            sum += (int) S.get();
        }
        return sum;
    }

    static long staticSetNull(H h, int n) {
        long seen = 0;
        for (int k = 0; k < n; k++) {
            SR.set((Object) null);
            if (sr != null) {
                seen++;
            }
        }
        return seen;
    }

    static long getAndAddInt(H h, int n) {
        long sum = 0;
        h.i = 0;
        for (int k = 0; k < n; k++) {
            h.i = k;
            sum += (int) I.getAndAdd(h, 1);
        }
        return sum;
    }

    static long getAndSetRef(H h, int n) {
        long seen = 0;
        Object o = "y";
        for (int k = 0; k < n; k++) {
            if ((Object) R.getAndSet(h, o) != null) {
                seen++;
            }
        }
        return seen;
    }

    static long caeInt(H h, int n) {
        long sum = 0;
        for (int k = 0; k < n; k++) {
            h.i = k;
            sum += (int) I.compareAndExchange(h, k, k + 1);
        }
        return sum;
    }

    static void row(String name, Row r) {
        r.run(new H(), WARMUP);
        H h = new H();
        long t0 = System.nanoTime();
        long check = r.run(h, ITERS);
        long t1 = System.nanoTime();
        System.out.println(name + " " + check);
        System.err.printf("%s %.1f ns/op%n", name, (t1 - t0) / (double) ITERS);
    }

    public static void main(String[] args) {
        row("get-int", L4W47VarHandleNativeBench::getInt);
        row("set-int", L4W47VarHandleNativeBench::setInt);
        row("cas-int", L4W47VarHandleNativeBench::casInt);
        row("get-ref", L4W47VarHandleNativeBench::getRef);
        row("static-get-int", L4W47VarHandleNativeBench::staticGetInt);
        row("static-set-null", L4W47VarHandleNativeBench::staticSetNull);
        row("getandadd-int", L4W47VarHandleNativeBench::getAndAddInt);
        row("getandset-ref", L4W47VarHandleNativeBench::getAndSetRef);
        row("cae-int", L4W47VarHandleNativeBench::caeInt);
    }
}
