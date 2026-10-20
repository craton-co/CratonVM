import java.util.function.IntSupplier;

/**
 * SBR5 interp lane: per-bytecode-class interpreter cost with every kernel in its OWN small method
 * (at most 4 locals, so the iload_N/istore_N fusion arms apply), the counterpart of Sbr5InterpOps
 * whose one big run() method has 18 locals. usage: Sbr5InterpKern mode|all [n]. Prints "mode ns/op".
 */
public class Sbr5InterpKern {
    interface I { int f(int x); }
    static final class A implements I { int v; public int f(int x) { return x + 1; } int g(int x) { return x + v; } private int p(int x) { return x + 3; } int callPriv(int x) { return p(x); } }
    static final class B implements I { public int f(int x) { return x + 2; } }
    static class Base { int m(int x) { return x + 1; } }
    static class Derived extends Base { int m(int x) { return x + 2; } int sup(int x) { return super.m(x); } }
    static int sf(int x) { return x + 1; }
    static int sf3(int x, int y, int z) { return x + y + z; }
    static void svoid() { }
    static int sink, fld;
    int inst; long lfld; final Object lock = new Object();
    static int thrower(int i) { if ((i & 0) == 0) throw new IllegalStateException(); return i; }
    static int fact(int n) { return n <= 1 ? 1 : n * fact(n - 1); }

    static int empty(int n) { int s = 0; for (int i = 0; i < n; i++) { s += i; } return s; }
    static int arith(int n) { int s = 0; for (int i = 0; i < n; i++) { s = (s * 31 + i) ^ (s >>> 3); } return s; }
    static int stat(int n) { int s = 0; for (int i = 0; i < n; i++) { s = sf(s); } return s; }
    static int stat3(int n) { int s = 0; for (int i = 0; i < n; i++) { s = sf3(s, i, 1); } return s; }
    static int statvoid(int n) { for (int i = 0; i < n; i++) { svoid(); } return 0; }
    static int virt(A a, int n) { int s = 0; for (int i = 0; i < n; i++) { s = a.g(s); } return s; }
    static int virtpoly(Base b, int n) { int s = 0; for (int i = 0; i < n; i++) { s = b.m(s); } return s; }
    static int specpriv(A a, int n) { int s = 0; for (int i = 0; i < n; i++) { s = a.callPriv(s); } return s; }
    static int specsuper(Derived d, int n) { int s = 0; for (int i = 0; i < n; i++) { s = d.sup(s); } return s; }
    static int iface(I a, I b, int n) { int s = 0; for (int i = 0; i < n; i++) { s = ((i & 1) == 0 ? a : b).f(s); } return s; }
    static int ifacemono(I a, int n) { int s = 0; for (int i = 0; i < n; i++) { s = a.f(s); } return s; }
    static int getf(Sbr5InterpKern o, int n) { int s = 0; for (int i = 0; i < n; i++) { s += o.inst; } return s; }
    static int putf(Sbr5InterpKern o, int n) { for (int i = 0; i < n; i++) { o.inst = i; } return 0; }
    static int getput(Sbr5InterpKern o, int n) { for (int i = 0; i < n; i++) { o.lfld += i; } return 0; }
    static int gets(int n) { int s = 0; for (int i = 0; i < n; i++) { s += fld; } return s; }
    static int puts(int n) { for (int i = 0; i < n; i++) { fld = i; } return 0; }
    static int alloc(int n) { int s = 0; for (int i = 0; i < n; i++) { s += new A().v; } return s; }
    static int newarr(int n) { int s = 0; for (int i = 0; i < n; i++) { s += new int[4].length; } return s; }
    static int arr(int[] a, int n) { int s = 0; for (int i = 0; i < n; i++) { a[i & 63] += i; s += a[(i + 1) & 63]; } return s; }
    static int objarr(Object[] a, Object o, int n) { int s = 0; for (int i = 0; i < n; i++) { a[i & 63] = o; s += a[(i + 1) & 63] == null ? 1 : 0; } return s; }
    static int chk(Object o, int n) { int s = 0; for (int i = 0; i < n; i++) { s += ((A) o).v; } return s; }
    static int inst(Object o, Object str, int n) { int s = 0; for (int i = 0; i < n; i++) { if (o instanceof I) s++; if (str instanceof A) s--; } return s; }
    static int sync(Sbr5InterpKern o, int n) { int s = 0; for (int i = 0; i < n; i++) { synchronized (o.lock) { s++; } } return s; }
    static int concat(int n) { int s = 0; for (int i = 0; i < n; i++) { s += ("a" + i).length(); } return s; }
    static int lambda(IntSupplier sup, int n) { int s = 0; for (int i = 0; i < n; i++) { s += sup.getAsInt(); } return s; }
    static int exc(int n) { int s = 0; for (int i = 0; i < n; i++) { try { s += thrower(i); } catch (IllegalStateException e) { s++; } } return s; }
    static int recur(int n) { int s = 0; for (int i = 0; i < n; i += 10) { s += fact(10); } return s; }
    static int str(int n) { int s = 0; for (int i = 0; i < n; i++) { s += Integer.toString(i).length(); } return s; }
    static int hmap(int n) { java.util.HashMap<Integer,Integer> m = new java.util.HashMap<>(); for (int i = 0; i < n; i++) { m.merge(i & 255, 1, Integer::sum); } return m.size(); }
    static int alist(int n) { java.util.ArrayList<Integer> l = new java.util.ArrayList<>(); for (int i = 0; i < n; i++) { l.add(i); if (l.size() > 1000) l.clear(); } return l.size(); }

    static long run(String mode, int n) {
        A a = new A(); I ia = a; I ib = new B(); Base bb = new Derived(); Derived dd = new Derived();
        Sbr5InterpKern o = new Sbr5InterpKern(); int[] ar = new int[64]; Object[] oa = new Object[64];
        IntSupplier sup = () -> 7;
        long t0 = System.nanoTime();
        int s;
        switch (mode) {
            case "empty": s = empty(n); break;
            case "arith": s = arith(n); break;
            case "static": s = stat(n); break;
            case "static3": s = stat3(n); break;
            case "staticvoid": s = statvoid(n); break;
            case "virtual": s = virt(a, n); break;
            case "virtual_poly": s = virtpoly(bb, n); break;
            case "special_priv": s = specpriv(a, n); break;
            case "special_super": s = specsuper(dd, n); break;
            case "interface": s = iface(ia, ib, n); break;
            case "interface_mono": s = ifacemono(ia, n); break;
            case "getfield": s = getf(o, n); break;
            case "putfield": s = putf(o, n); break;
            case "getputlong": s = getput(o, n); break;
            case "getstatic": s = gets(n); break;
            case "putstatic": s = puts(n); break;
            case "alloc": s = alloc(n); break;
            case "newarray": s = newarr(n); break;
            case "array": s = arr(ar, n); break;
            case "objarray": s = objarr(oa, a, n); break;
            case "checkcast": s = chk(a, n); break;
            case "instanceof": s = inst(a, "x", n); break;
            case "sync": s = sync(o, n); break;
            case "concat": s = concat(n); break;
            case "lambda": s = lambda(sup, n); break;
            case "exception": s = exc(n); break;
            case "recursion": s = recur(n); break;
            case "string": s = str(n); break;
            case "hashmap": s = hmap(n); break;
            case "arraylist": s = alist(n); break;
            default: throw new IllegalArgumentException(mode);
        }
        long t = System.nanoTime() - t0;
        sink += s;
        return t;
    }

    static final String[] ALL = { "empty", "arith", "static", "static3", "staticvoid", "virtual", "virtual_poly", "special_priv", "special_super", "interface", "interface_mono",
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
