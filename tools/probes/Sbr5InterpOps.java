import java.util.function.IntSupplier;

/**
 * SBR5 interp lane: per-bytecode-class interpreter cost. usage: Sbr5InterpOps <mode|all> [n].
 * Run with --nojit (CratonVM) / -Xint (HotSpot). Prints "mode ns/op" (min of 5 timed runs;
 * the empty-loop row is NOT subtracted, so ratios of the empty row show loop overhead).
 */
public class Sbr5InterpOps {
    interface I { int f(int x); }
    static final class A implements I { int v; public int f(int x) { return x + 1; } int g(int x) { return x + v; } private int p(int x) { return x + 3; } int callPriv(int x) { return p(x); } }
    static final class B implements I { public int f(int x) { return x + 2; } }
    static class Base { int m(int x) { return x + 1; } }
    static class Derived extends Base { int m(int x) { return x + 2; } int sup(int x) { return super.m(x); } }
    static int sf(int x) { return x + 1; }
    static int sf3(int x, int y, int z) { return x + y + z; }
    static void svoid() { }
    static int sink;
    static int fld;
    int inst;
    long lfld;
    final Object lock = new Object();

    static int thrower(int i) { if ((i & 0) == 0) throw new IllegalStateException(); return i; }
    static int fact(int n) { return n <= 1 ? 1 : n * fact(n - 1); }

    static long run(String mode, int n) {
        int s = 0;
        A a = new A(); I ia = a; I ib = new B();
        Base bb = new Derived(); Derived dd = new Derived();
        Object o0 = a; Object str = "x";
        Sbr5InterpOps o = new Sbr5InterpOps();
        long t0 = System.nanoTime();
        switch (mode) {
            case "empty": for (int i = 0; i < n; i++) { s += i; } break;
            case "arith": { long l = 1; for (int i = 0; i < n; i++) { s = (s * 31 + i) ^ (s >>> 3); l += s; } s += (int) l; break; }
            case "static": for (int i = 0; i < n; i++) { s = sf(s); } break;
            case "static3": for (int i = 0; i < n; i++) { s = sf3(s, i, 1); } break;
            case "staticvoid": for (int i = 0; i < n; i++) { svoid(); } break;
            case "virtual": for (int i = 0; i < n; i++) { s = a.g(s); } break;
            case "virtual_poly": for (int i = 0; i < n; i++) { s = bb.m(s); } break;
            case "special_priv": for (int i = 0; i < n; i++) { s = a.callPriv(s); } break;
            case "special_super": for (int i = 0; i < n; i++) { s = dd.sup(s); } break;
            case "interface": for (int i = 0; i < n; i++) { s = ((i & 1) == 0 ? ia : ib).f(s); } break;
            case "getfield": for (int i = 0; i < n; i++) { s += o.inst; } break;
            case "putfield": for (int i = 0; i < n; i++) { o.inst = i; } break;
            case "getputlong": for (int i = 0; i < n; i++) { o.lfld += i; } break;
            case "getstatic": for (int i = 0; i < n; i++) { s += fld; } break;
            case "putstatic": for (int i = 0; i < n; i++) { fld = i; } break;
            case "alloc": for (int i = 0; i < n; i++) { s += new A().v; } break;
            case "newarray": for (int i = 0; i < n; i++) { s += new int[4].length; } break;
            case "array": { int[] arr = new int[64]; for (int i = 0; i < n; i++) { arr[i & 63] += i; s += arr[(i + 1) & 63]; } break; }
            case "objarray": { Object[] arr = new Object[64]; for (int i = 0; i < n; i++) { arr[i & 63] = a; s += arr[(i + 1) & 63] == null ? 1 : 0; } break; }
            case "checkcast": for (int i = 0; i < n; i++) { s += ((A) o0).v; } break;
            case "instanceof": for (int i = 0; i < n; i++) { if (o0 instanceof I) s++; if (str instanceof A) s--; } break;
            case "sync": for (int i = 0; i < n; i++) { synchronized (o.lock) { s++; } } break;
            case "concat": for (int i = 0; i < n; i++) { s += ("a" + i).length(); } break;
            case "lambda": { IntSupplier sup = () -> 7; for (int i = 0; i < n; i++) { s += sup.getAsInt(); } break; }
            case "exception": for (int i = 0; i < n; i++) { try { s += thrower(i); } catch (IllegalStateException e) { s++; } } break;
            case "recursion": for (int i = 0; i < n; i += 10) { s += fact(10); } break;
            case "string": for (int i = 0; i < n; i++) { s += Integer.toString(i).length(); } break;
            case "hashmap": { java.util.HashMap<Integer,Integer> m = new java.util.HashMap<>(); for (int i = 0; i < n; i++) { m.merge(i & 255, 1, Integer::sum); } s = m.size(); break; }
            case "arraylist": { java.util.ArrayList<Integer> l = new java.util.ArrayList<>(); for (int i = 0; i < n; i++) { l.add(i); if (l.size() > 1000) l.clear(); } s = l.size(); break; }
            default: throw new IllegalArgumentException(mode);
        }
        long t = System.nanoTime() - t0;
        sink += s;
        return t;
    }

    static final String[] ALL = { "empty", "arith", "static", "static3", "staticvoid", "virtual", "virtual_poly", "special_priv", "special_super", "interface",
        "getfield", "putfield", "getputlong", "getstatic", "putstatic", "alloc", "newarray", "array", "objarray", "checkcast", "instanceof", "sync", "concat",
        "lambda", "exception", "recursion", "string", "hashmap", "arraylist" };

    public static void main(String[] args) {
        String mode = args.length > 0 ? args[0] : "all";
        int n = args.length > 1 ? Integer.parseInt(args[1]) : 1_000_000;
        String[] modes = mode.equals("all") ? ALL : new String[] { mode };
        for (String m : modes) {
            int nn = (m.equals("exception")) ? Math.max(n / 20, 1000) : (m.equals("string") || m.equals("hashmap") || m.equals("concat") || m.equals("arraylist")) ? Math.max(n / 10, 1000) : n;
            run(m, Math.min(nn, 3000));
            long best = Long.MAX_VALUE;
            for (int r = 0; r < 5; r++) best = Math.min(best, run(m, nn));
            System.out.println(m + " " + String.format("%.1f", best / (double) nn));
        }
        if (sink == 42) System.out.println("x");
    }
}
