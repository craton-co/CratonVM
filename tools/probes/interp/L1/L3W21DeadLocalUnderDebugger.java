// Interpreter round i1 wave 21, lane L3: a dead but assigned local in a
// compiled loop, and a guard that fails mid-loop.
//
// Plain run (what the probe runner diffs): stdout is deterministic and must
// equal HotSpot 25's:
//
//   sum=499500 t=42 kinds=2
//
// By hand, under a debugger (the case the wave fixes; the runner cannot attach
// one): run with the JDWP agent, e.g.
//   -agentlib:jdwp=transport=dt_socket,server=y,suspend=y,address=5005
// and `jdb -attach 5005`, then `stop at L3W21DeadLocalUnderDebugger:50`
// (the `return` after the loop; compile with `javac -g`) and `run`; at the
// stop, `locals`.
// HotSpot shows `t = 42` (it keeps every local alive while an agent can access
// locals). CratonVM showed `t = 0` when the loop's compiled body deoptimized
// at the failing receiver guard, and (since wave 19) refused to leave the
// compiled loop for the breakpoint at all; since wave 21 a debugger's
// single-pass compile keeps `t` in its home and every deopt point describes
// it, so the loop leaves and `t = 42` shows. (Since wave 22, lane L2, an
// optimizing-tier body that would show `t` as 0 is not installed under a
// debugger; the single-pass body runs instead:
// docs/internal/fixed-bugs/interpreter-L2-deopt-resumed-frames-show-dead-locals-as-zero-to-a-debugger-FIXED-20260926.md.)
public class L3W21DeadLocalUnderDebugger {
    interface Shape {
        int weight();
    }

    static final class Square implements Shape {
        public int weight() {
            return 1;
        }
    }

    static final class Circle implements Shape {
        public int weight() {
            return 2;
        }
    }

    static int run(Shape[] shapes) {
        int t = 42; // assigned, never read again
        int sum = 0;
        for (int i = 0; i < 1000; i++) {
            sum += i;
            // A receiver-type guard the loop's compiled body speculates on
            // until the second class arrives near the end.
            if (shapes[i].weight() > 5) {
                sum = -1;
            }
        }
        return sum; // `t` is in scope here, and dead
    }

    public static void main(String[] args) {
        Shape[] shapes = new Shape[1000];
        for (int i = 0; i < shapes.length; i++) {
            shapes[i] = i < 990 ? new Square() : new Circle();
        }
        int sum = 0;
        for (int rep = 0; rep < 200; rep++) {
            sum = run(shapes);
        }
        int kinds = 0;
        boolean sawSquare = false;
        boolean sawCircle = false;
        for (Shape s : shapes) {
            sawSquare |= s instanceof Square;
            sawCircle |= s instanceof Circle;
        }
        kinds = (sawSquare ? 1 : 0) + (sawCircle ? 1 : 0);
        System.out.println("sum=" + sum + " t=42 kinds=" + kinds);
    }
}
