// j27 wave 7 (lane T1L1, CF23-1 of
// docs/feature-designs/jit-ir-optimizations.md and
// docs/feature-designs/jit-ir-optimizations.md):
// nested bodies inside a SPLICED callee's covered `try`, and allocations there
// under a catch type that cannot catch an Error.
//
// Each row is a static "caller" method that calls a small "callee" with a
// try/catch; the callee is the body the optimizing tier splices with its
// handler rows, and the call / allocation inside the callee's try is the
// covered nested row. The catch paths are run AFTER the warm loops, so a
// compiled caller (or its deopt) must still deliver them into the callee's
// catch exactly as the interpreter does.
//
// Switches under test (all opt-in, j27 wave 7):
//   CRATONVM_JIT_IR_SPLICE_COVERED_NESTED_GUARDED=1
//   CRATONVM_JIT_IR_SPLICE_HANDLER_LOWER_PROVEN=1
//   CRATONVM_JIT_IR_SPLICE_COVERED_NEW_NON_ERROR_CATCH=1
//   (plus CRATONVM_JIT_IR_SPLICE_TAIL_HANDLER=1 for the wrap shapes)
//
//   java -cp <dir> J27T1L1CoveredNestedRows > hs.txt
//   cratonvm --cp <dir> J27T1L1CoveredNestedRows | diff - hs.txt   (every arm, --compatible, --nojit)
//   java -Dj27.time=true -cp <dir> J27T1L1CoveredNestedRows      (adds `time <row> <ns/op>` lines)
//
// Expected HotSpot 25 output (facts only; N = 200000 per rep, 5 reps):
//   fact make-sum 20000100000
//   fact check-sum 102187360
//   fact set-sum 19999900000
//   fact wrap-sum 1600000
//   fact set-null -1
//   fact check-neg caught=1000 cls=java.lang.IndexOutOfBoundsException msg=wrapped neg
//   fact wrap-bad java.lang.IndexOutOfBoundsException msg=null
//   fact make-bad java.lang.IndexOutOfBoundsException
//   fact ctor-error java.lang.StackOverflowError-not-caught-by-IAE
//
// Derivations: make-sum = sum(i+1, i<N) = N(N+1)/2; check-sum = sum(i & 1023,
// i<N) = 195 * 523776 + sum(0..319) = 102136320 + 51040; set-sum = N(N-1)/2;
// wrap-sum = 8 * N (remaining() of a wrap of length 8). wrap(arr, -1, 2):
// Buffer.position(-1) raises IllegalArgumentException, which wrap turns into
// a message-less IndexOutOfBoundsException. make(-1, 0): Sub's Base.<init>
// rejects a negative value with IllegalArgumentException, make wraps it.
// ctor-error: an Error raised in a constructor under catch
// (IllegalArgumentException) is not caught there and reaches the caller.
import java.nio.ByteBuffer;

public class J27T1L1CoveredNestedRows {
    static final int N = 200_000;
    static final int REPS = 5;
    static final boolean TIME = Boolean.getBoolean("j27.time");

    static class Base {
        int a;

        Base(int a) {
            if (a < 0) {
                throw new IllegalArgumentException("neg");
            }
            this.a = a;
        }
    }

    static final class Sub extends Base {
        int b;

        Sub(int a, int b) {
            super(a);
            this.b = b;
        }
    }

    static final class Bad {
        final int v;

        Bad(int v) {
            if (v < 0) {
                throw new IllegalArgumentException("neg");
            }
            this.v = v;
        }
    }

    static final class Deep {
        final int v;

        Deep(int v) {
            if (v == Integer.MIN_VALUE) {
                throw new StackOverflowError("synthetic");
            }
            this.v = v;
        }
    }

    static final class Box {
        int v;

        void set(int v) {
            this.v = v;
        }
    }

    // ── callees (the bodies spliced with their handler rows) ────────────

    /// The wrap shape: `try { return new X(..); } catch (IAE) { throw new IOOBE(); }`.
    static Sub make(int a, int b) {
        try {
            return new Sub(a, b);
        } catch (IllegalArgumentException x) {
            throw new IndexOutOfBoundsException();
        }
    }

    static Bad check(int v) {
        try {
            return new Bad(v);
        } catch (IllegalArgumentException x) {
            throw new IndexOutOfBoundsException("wrapped " + x.getMessage());
        }
    }

    static Deep deep(int v) {
        try {
            return new Deep(v);
        } catch (IllegalArgumentException x) {
            throw new IndexOutOfBoundsException("wrong handler");
        }
    }

    /// A nested setter body inside the covered try.
    static int setInTry(Box box, int v) {
        try {
            box.set(v);
            return box.v;
        } catch (RuntimeException e) {
            return -1;
        }
    }

    // ── callers (the compiled methods) ───────────────────────────────────

    static long makeRow(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += make(i, i + 1).b;
        }
        return s;
    }

    static long checkRow(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += check(i & 1023).v;
        }
        return s;
    }

    /// Throws at its first call (`check(-1)`); called 1000 times, so the
    /// throw comes from a compiled caller once it is hot.
    static long checkNegRow() {
        long s = 0;
        for (int i = 0; i < 4; i++) {
            s += check(i - 1).v;
        }
        return s;
    }

    static long setRow(Box box, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += setInTry(box, i);
        }
        return s;
    }

    static long wrapRow(byte[] arr, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += ByteBuffer.wrap(arr, i & 7, 8).remaining();
        }
        return s;
    }

    static long deepRow(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += deep(i).v;
        }
        return s;
    }

    public static void main(String[] args) {
        byte[] arr = new byte[16];
        Box box = new Box();
        long make = 0, check = 0, set = 0, wrap = 0;
        for (int r = 0; r < REPS; r++) {
            long t0 = System.nanoTime();
            make = makeRow(N);
            long t1 = System.nanoTime();
            check = checkRow(N);
            long t2 = System.nanoTime();
            set = setRow(box, N);
            long t3 = System.nanoTime();
            wrap = wrapRow(arr, N);
            long t4 = System.nanoTime();
            deepRow(N);
            if (TIME && r == REPS - 1) {
                System.out.println("time make " + (t1 - t0) / N);
                System.out.println("time check " + (t2 - t1) / N);
                System.out.println("time set " + (t3 - t2) / N);
                System.out.println("time wrap " + (t4 - t3) / N);
            }
        }
        System.out.println("fact make-sum " + make);
        System.out.println("fact check-sum " + check);
        System.out.println("fact set-sum " + set);
        System.out.println("fact wrap-sum " + wrap);
        // The catch paths, from the same (compiled) callers.
        System.out.println("fact set-null " + setRow(null, 1));
        int caught = 0;
        String cls = "none";
        String msg = "none";
        for (int i = 0; i < 1000; i++) {
            try {
                checkNegRow();
            } catch (IndexOutOfBoundsException e) {
                caught++;
                cls = e.getClass().getName();
                msg = e.getMessage();
            }
        }
        System.out.println("fact check-neg caught=" + caught + " cls=" + cls + " msg=" + msg);
        try {
            ByteBuffer.wrap(arr, -1, 2);
            System.out.println("fact wrap-bad none");
        } catch (RuntimeException e) {
            System.out.println("fact wrap-bad " + e.getClass().getName() + " msg=" + e.getMessage());
        }
        try {
            make(-1, 0);
            System.out.println("fact make-bad none");
        } catch (RuntimeException e) {
            System.out.println("fact make-bad " + e.getClass().getName());
        }
        try {
            deep(Integer.MIN_VALUE);
            System.out.println("fact ctor-error none");
        } catch (IndexOutOfBoundsException e) {
            System.out.println("fact ctor-error caught-by-the-wrong-handler");
        } catch (StackOverflowError e) {
            System.out.println("fact ctor-error java.lang.StackOverflowError-not-caught-by-IAE");
        }
    }
}
