// JIT round j27 wave 7, lane T1LAM3 (book docs/feature-designs/jit-ir-optimizations.md,
// LAMBDACAP21-3): split capture-create-only's per-allocation cost. Each row's kernel makes 16 objects that
// escape (a static or a field), so HotSpot allocates too and no lambda plan can remove them; the rows
// differ only in WHAT is allocated:
//   capture-create-only   a two-capture lambda proxy stored to a static (the book's row)
//   pair-create-only      a plain two-int object stored to a static (allocation control)
//   long4-create-only     a plain four-long object (about the proxy's 48-byte legacy-cell size)
//   word-create-only      a non-capturing lambda (published singleton: no allocation) to a static
//   capture-create-field  the proxy stored to an instance field instead of a static
// capture-create-only minus pair-create-only / long4-create-only is the proxy's own premium; word-create-only
// is the loop + putstatic floor.
//
// Run lines (build: javac -d tools/probes/out tools/probes/jit/j27/J27T1LambdacapCreateRows.java):
//   P=tools/probes/out
//   java -Dj27.time=true -cp $P J27T1LambdacapCreateRows                 # HotSpot 25
//   cratonvm -Dj27.time=true -cp $P J27T1LambdacapCreateRows             # jdk-only
//   cratonvm --compatible -Dj27.time=true -cp $P J27T1LambdacapCreateRows
//   CRATONVM_GC_STATS=1 cratonvm --verbose:gc -Dj27.time=true -cp $P J27T1LambdacapCreateRows   # pauses per row
//   perf record -e cpu-clock -g -- cratonvm -Dj27.time=true -Dj27.row=capture-create-only -cp $P J27T1LambdacapCreateRows
// -Dj27.row=<name> runs one row alone (for perf). Time lines are ns per kernel call (16 allocations).
//
// Expected HotSpot 25 output (fact lines; N = 200000 calls, a = call % 1000, every kernel returns a + 15):
//   fact capture-create-only 102900000
//   fact pair-create-only 102900000
//   fact long4-create-only 102900000
//   fact word-create-only 102900000
//   fact capture-create-field 102900000
//   fact capture-fresh 1        (two evaluations of a capturing lambda are distinct objects on HotSpot)
//   fact word-same 1            (a non-capturing site yields one instance on HotSpot)

import java.util.function.IntSupplier;

public class J27T1LambdacapCreateRows {
    static final int N = 200_000;
    static Object esc;

    static final class Pair {
        final int x;
        final int y;

        Pair(int x, int y) {
            this.x = x;
            this.y = y;
        }
    }

    static final class Long4 {
        final long a;
        final long b;
        final long c;
        final long d;

        Long4(long a, long b, long c, long d) {
            this.a = a;
            this.b = b;
            this.c = c;
            this.d = d;
        }
    }

    static final class Holder {
        IntSupplier f;
    }

    static final Holder HOLDER = new Holder();

    static int captureCreateOnly(int a) {
        for (int i = 0; i < 16; i++) {
            int k = i;
            IntSupplier f = () -> a + k;
            esc = f;
        }
        return ((IntSupplier) esc).getAsInt();
    }

    static int pairCreateOnly(int a) {
        for (int i = 0; i < 16; i++) {
            esc = new Pair(a, i);
        }
        Pair p = (Pair) esc;
        return p.x + p.y;
    }

    static int long4CreateOnly(int a) {
        for (int i = 0; i < 16; i++) {
            esc = new Long4(a, i, 0L, 0L);
        }
        Long4 p = (Long4) esc;
        return (int) (p.a + p.b + p.c + p.d);
    }

    static int wordCreateOnly(int a) {
        for (int i = 0; i < 16; i++) {
            IntSupplier f = () -> 15;
            esc = f;
        }
        return ((IntSupplier) esc).getAsInt() + a;
    }

    static int captureCreateField(int a) {
        Holder h = HOLDER;
        for (int i = 0; i < 16; i++) {
            int k = i;
            h.f = () -> a + k;
        }
        return h.f.getAsInt();
    }

    static IntSupplier capturing(int k) {
        return () -> k;
    }

    static IntSupplier word() {
        return () -> 1;
    }

    interface Kernel {
        int run(int a);
    }

    static void row(String name, Kernel kernel) {
        String only = System.getProperty("j27.row");
        if (only != null && !only.equals(name)) {
            return;
        }
        long sum = 0;
        long t0 = 0;
        for (int rep = 0; rep < 3; rep++) {
            if (rep == 2) {
                t0 = System.nanoTime();
            }
            sum = 0;
            for (int j = 0; j < N; j++) {
                sum += kernel.run(j % 1000);
            }
        }
        long t1 = System.nanoTime();
        System.out.println("fact " + name + " " + sum);
        if (Boolean.getBoolean("j27.time")) {
            System.out.println("time " + name + " " + (t1 - t0) / N + " ns/call");
        }
    }

    public static void main(String[] args) {
        row("capture-create-only", J27T1LambdacapCreateRows::captureCreateOnly);
        row("pair-create-only", J27T1LambdacapCreateRows::pairCreateOnly);
        row("long4-create-only", J27T1LambdacapCreateRows::long4CreateOnly);
        row("word-create-only", J27T1LambdacapCreateRows::wordCreateOnly);
        row("capture-create-field", J27T1LambdacapCreateRows::captureCreateField);
        if (System.getProperty("j27.row") == null) {
            System.out.println("fact capture-fresh " + (capturing(3) != capturing(3) ? 1 : 0));
            System.out.println("fact word-same " + (word() == word() ? 1 : 0));
        }
    }
}
