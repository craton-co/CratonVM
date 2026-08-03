// Minimal-reproducer attempt for the `CRATONVM_JIT=deopt-real=0` SIGSEGV.
//
// Hypothesis under test: a compiled `invokedynamic` lowers to an unconditional
// trap whose resolution path is `deopt_real`-gated, so with the gate off the
// trap jumps somewhere that was never emitted.
//
//   cratonvm --java-home <jdk> -cp probes IndyDeoptProbe          # control
//   CRATONVM_JIT='deopt-real=0' cratonvm … IndyDeoptProbe         # suspect
//
// Reused by `loop-02` for a second purpose: `concatLoop` is a loop with an
// `invokedynamic` INSIDE it, which is the shape that says whether the bytecode
// loop rewriter's bci translation holds for the one snapshot path that is not
// gated on `deopt_real`. Run it armed —
// `CRATONVM_JIT='bytecode-loop-xform'` — and it must still match `java`. It is
// also the method whose two unrolled copies legitimately disagree about local
// 3's oop-ness at the indy, which is what scoped
// `rewritten_deopt_points_are_publishable`'s copy-agreement check.
public class IndyDeoptProbe {

    interface F {
        int f(int x);
    }

    // `invokedynamic` (LambdaMetafactory) inside a method hot enough to compile.
    static int lambdaLoop(int n) {
        F f = x -> x * 3 + 1;
        int s = 0;
        for (int i = 0; i < n; i++) {
            s += f.f(i);
        }
        return s;
    }

    // `invokedynamic` (StringConcatFactory) in a hot method — the other indy
    // shape javac emits without anyone asking for it.
    static int concatLoop(int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            String t = "v" + i;
            s += t.length();
        }
        return s;
    }

    public static void main(String[] args) {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 50000;
        long acc = 0;
        for (int r = 0; r < reps; r++) {
            acc += lambdaLoop(40);
        }
        System.out.println("lambda=" + acc);
        acc = 0;
        for (int r = 0; r < reps; r++) {
            acc += concatLoop(40);
        }
        System.out.println("concat=" + acc);
    }
}
