import java.util.function.IntSupplier;

/** SBR5 warmup lane: interpreter per-op cost. usage: Sbr5WarmupCalls <mode> [n]. Run with --nojit / -Xint for interpreter cost. */
public class Sbr5WarmupCalls {
    interface I { int f(int x); }
    static final class A implements I { int v; public int f(int x) { return x + 1; } int g(int x) { return x + v; } }
    static final class B implements I { public int f(int x) { return x + 2; } }
    static int sf(int x) { return x + 1; }
    static int sink;
    static int fld;
    int inst;

    static long run(String mode, int n) {
        int s = 0;
        A a = new A(); I ia = a; I ib = new B();
        Sbr5WarmupCalls o = new Sbr5WarmupCalls();
        long t0 = System.nanoTime();
        switch (mode) {
            case "empty": for (int i = 0; i < n; i++) { s += i; } break;
            case "static": for (int i = 0; i < n; i++) { s = sf(s); } break;
            case "virtual": for (int i = 0; i < n; i++) { s = a.g(s); } break;
            case "interface": for (int i = 0; i < n; i++) { s = ((i & 1) == 0 ? ia : ib).f(s); } break;
            case "getfield": for (int i = 0; i < n; i++) { s += o.inst; } break;
            case "putfield": for (int i = 0; i < n; i++) { o.inst = i; } break;
            case "getstatic": for (int i = 0; i < n; i++) { s += fld; } break;
            case "alloc": for (int i = 0; i < n; i++) { s += new A().v; } break;
            case "array": { int[] arr = new int[64]; for (int i = 0; i < n; i++) { arr[i & 63] += i; s += arr[(i + 1) & 63]; } break; }
            case "string": for (int i = 0; i < n; i++) { s += Integer.toString(i).length(); } break;
            case "hashmap": { java.util.HashMap<Integer,Integer> m = new java.util.HashMap<>(); for (int i = 0; i < n; i++) { m.merge(i & 255, 1, Integer::sum); } s = m.size(); break; }
            case "lambda": { IntSupplier sup = () -> 7; for (int i = 0; i < n; i++) { s += sup.getAsInt(); } break; }
            default: throw new IllegalArgumentException(mode);
        }
        long t = System.nanoTime() - t0;
        sink += s;
        return t;
    }

    public static void main(String[] args) {
        String mode = args[0];
        int n = args.length > 1 ? Integer.parseInt(args[1]) : 2_000_000;
        long cold = run(mode, Math.min(n, 2000));
        long best = Long.MAX_VALUE;
        for (int r = 0; r < 3; r++) best = Math.min(best, run(mode, n));
        System.out.println("ns/op " + mode + " " + String.format("%.1f", best / (double) n) + " cold2000=" + String.format("%.1f", cold / 2000.0) + " sink=" + sink);
    }
}
