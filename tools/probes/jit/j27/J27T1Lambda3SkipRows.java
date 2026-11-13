// JIT round j27 wave 7, lane T1LAM3 (book docs/feature-designs/jit-ir-optimizations.md,
// LAMBDA321-3): every lambda `invokedynamic` site of an optimizing-tier compile either gets an own-site
// impl-call plan (`[ir] lambda-impl-site` census) or a reason (`[ir] lambda-impl-skip ... reason=`).
// The census is debug output only; the `fact` lines check the rows compute the same answers in every arm.
//
// Run lines (build: javac -d tools/probes/out tools/probes/jit/j27/J27T1Lambda3SkipRows.java):
//   P=tools/probes/out
//   java -cp $P J27T1Lambda3SkipRows                                   # HotSpot 25
//   cratonvm -cp $P J27T1Lambda3SkipRows                               # jdk-only
//   cratonvm --compatible -cp $P J27T1Lambda3SkipRows                  # compatible
//   CRATONVM_DBG_JITC=1 cratonvm -cp $P J27T1Lambda3SkipRows 2>&1 \
//     | grep -E '\[ir\] lambda-impl-(skip|site) J27T1Lambda3SkipRows' | sed -E 's/pc=[0-9]+/pc=N/' | sort | uniq -c
// Expected census (one compile of each row method; pcs vary):
//   nonCapturing / capturing: a `lambda-impl-site` line each, no skip line
//   concat:   `lambda-impl-skip ... reason=no-capture-fact` (or capture-new-off / no-impl-answer:
//             a string-concat site is no lambda, any of these three is right; never a plan)
//   identity: `reason=no-planned-call` (the lambda is never applied)
//   passed:   `reason=no-planned-call` (the SAM call is in the callee, LAMBDA321-5)
// Arms (every `fact` line identical in each): default, CRATONVM_JIT_IR_LAMBDA_IMPL_CALL=0,
//   CRATONVM_JIT_IR_LAMBDA_IMPL_CAPTURING=0 (capturing row: `reason=capturing-off`), --nojit.
//
// Expected HotSpot 25 output (k = 0 .. 19999, int results summed as long):
//   fact non-capturing 200130000      (sum of 7 + k)
//   fact capturing 599970000          (sum of 3k)
//   fact concat 108890                (sum of ("x" + k).length() = 1 + digits(k))
//   fact identity 30000               (f == f is true: 20000, plus k % 2: 10000)
//   fact passed 200010000             (sum of k + 1)

import java.util.function.IntSupplier;

public class J27T1Lambda3SkipRows {
    static final int N = 20_000;

    static int nonCapturing(int k) {
        IntSupplier f = () -> 7;
        return f.getAsInt() + k;
    }

    static int capturing(int k) {
        IntSupplier f = () -> k * 3;
        return f.getAsInt();
    }

    static int concat(int k) {
        return ("x" + k).length();
    }

    static int identity(int k) {
        IntSupplier f = () -> 1;
        IntSupplier g = f;
        return (f == g ? 1 : 0) + k % 2;
    }

    static int apply(IntSupplier s) {
        return s.getAsInt();
    }

    static int passed(int k) {
        IntSupplier f = () -> k + 1;
        return apply(f);
    }

    interface Row {
        int at(int k);
    }

    static void row(String name, Row r) {
        long t0 = System.nanoTime();
        long sum = 0;
        for (int rep = 0; rep < 5; rep++) {
            sum = 0;
            for (int k = 0; k < N; k++) {
                sum += r.at(k);
            }
        }
        long t1 = System.nanoTime();
        System.out.println("fact " + name + " " + sum);
        if (Boolean.getBoolean("j27.time")) {
            System.out.println("time " + name + " " + (t1 - t0) / (5L * N) + " ns/call");
        }
    }

    public static void main(String[] args) {
        row("non-capturing", J27T1Lambda3SkipRows::nonCapturing);
        row("capturing", J27T1Lambda3SkipRows::capturing);
        row("concat", J27T1Lambda3SkipRows::concat);
        row("identity", J27T1Lambda3SkipRows::identity);
        row("passed", J27T1Lambda3SkipRows::passed);
    }
}
