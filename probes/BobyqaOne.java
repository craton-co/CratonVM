import org.apache.commons.math4.legacy.analysis.MultivariateFunction;
import org.apache.commons.math4.legacy.optim.*;
import org.apache.commons.math4.legacy.optim.nonlinear.scalar.*;
import org.apache.commons.math4.legacy.optim.nonlinear.scalar.noderiv.BOBYQAOptimizer;

/**
 * One `optimize()` call on the 12-dimensional Rosenbrock — the core of
 * `BOBYQAOptimizerTest.testRosen`, reduced to a `main` so a measurement is one
 * call and not a JUnit suite.
 *
 * Written for
 * `docs/known-issues/perf/bobyqa-numeric-kernel-is-80x-slower-than-hotspot-20260817.md`,
 * which is the page that corrects an earlier OSR-refusal diagnosis of the same
 * symptom. The reason it prints per-rep milliseconds rather than a total is that
 * the interesting comparison is HotSpot's 0.5 s against CratonVM's ~45 s with
 * the JIT on and ~52 s with `--nojit` — the `--nojit` arm is what proves the
 * cost is in COMPILED code, and it is the arm the original diagnosis skipped.
 *
 *   javac -nowarn -cp "$CP" -d . BobyqaOne.java
 *   java -cp ".;$CP" BobyqaOne 12 1
 *   cratonvm --java-home <jdk> --Xmx 1g -c ".;$CP" BobyqaOne 12 1
 *   cratonvm --java-home <jdk> --nojit --Xmx 1g -c ".;$CP" BobyqaOne 12 1
 */
public final class BobyqaOne {
    static double rosen(double[] x) {
        double f = 0;
        for (int i = 0; i < x.length - 1; i++) {
            f += 1e2 * (x[i] * x[i] - x[i + 1]) * (x[i] * x[i] - x[i + 1]) + (x[i] - 1.) * (x[i] - 1.);
        }
        return f;
    }

    public static void main(String[] args) {
        int dim = args.length > 0 ? Integer.parseInt(args[0]) : 12;
        int reps = args.length > 1 ? Integer.parseInt(args[1]) : 1;
        double[] start = new double[dim];
        java.util.Arrays.fill(start, 0.1);
        MultivariateFunction f = BobyqaOne::rosen;
        for (int r = 0; r < reps; r++) {
            long t0 = System.nanoTime();
            BOBYQAOptimizer opt = new BOBYQAOptimizer(2 * dim + 1);
            PointValuePair res = opt.optimize(new MaxEval(3000),
                    new ObjectiveFunction(f),
                    GoalType.MINIMIZE,
                    new InitialGuess(start),
                    SimpleBounds.unbounded(dim));
            long ms = (System.nanoTime() - t0) / 1_000_000;
            System.out.println("rep=" + r + " value=" + res.getValue() + " ms=" + ms);
        }
        System.out.println("OK");
    }
}
