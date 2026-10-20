import java.util.*;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.function.*;
import java.util.stream.IntStream;

/**
 * SBR5 interp lane: ns/op of common JDK idioms, each in its own small method, for finding paths
 * that cost far more than their bytecode count (the lambda call was 38x HotSpot -Xint, every
 * plain bytecode class is 3-10x). Run with --nojit (CratonVM) / -Xint (HotSpot).
 * usage: Sbr5InterpIdioms mode|all [n]
 */
public class Sbr5InterpIdioms {
    enum Color { RED, GREEN, BLUE }
    record Pt(int x, int y) { int sum() { return x + y; } }
    interface Shape { double area(); default double twice() { return 2 * area(); } }
    static final class Sq implements Shape { final double s; Sq(double s) { this.s = s; } public double area() { return s * s; } }
    static class Outer { private int secret = 3; class Inner { int peek() { return secret; } } }
    static int sink;
    static final List<Integer> LIST = new ArrayList<>(Arrays.asList(1, 2, 3, 4, 5, 6, 7, 8));
    static final int[] ARR = {1, 2, 3, 4, 5, 6, 7, 8};
    static final ThreadLocal<Integer> TL = ThreadLocal.withInitial(() -> 5);

    static int foreachList(int n) { int s = 0; for (int i = 0; i < n; i += 8) { for (Integer x : LIST) s += x; } return s; }
    static int foreachArr(int n) { int s = 0; for (int i = 0; i < n; i += 8) { for (int x : ARR) s += x; } return s; }
    static int sbAppend(int n) { StringBuilder sb = new StringBuilder(); int s = 0; for (int i = 0; i < n; i++) { sb.append('a').append(i); if (sb.length() > 200) { s += sb.length(); sb.setLength(0); } } return s; }
    static int switchStr(String[] keys, int n) { int s = 0; for (int i = 0; i < n; i++) { switch (keys[i & 3]) { case "alpha": s += 1; break; case "beta": s += 2; break; case "gamma": s += 3; break; default: s += 4; } } return s; }
    static int switchEnum(Color[] cs, int n) { int s = 0; for (int i = 0; i < n; i++) { switch (cs[i % 3]) { case RED: s += 1; break; case GREEN: s += 2; break; default: s += 3; } } return s; }
    static int tryFinally(int n) { int s = 0; for (int i = 0; i < n; i++) { try { s += i; } finally { s++; } } return s; }
    static int boxing(int n) { int s = 0; for (int i = 0; i < n; i++) { Integer b = i & 127; s += b; } return s; }
    static int boxingBig(int n) { int s = 0; for (int i = 0; i < n; i++) { Integer b = i; s += b; } return s; }
    static int recordAcc(Pt p, int n) { int s = 0; for (int i = 0; i < n; i++) { s += p.x() + p.sum(); } return s; }
    static int defaultMethod(Shape sh, int n) { int s = 0; for (int i = 0; i < n; i++) { s += (int) sh.twice(); } return s; }
    static int innerAccess(Outer.Inner in, int n) { int s = 0; for (int i = 0; i < n; i++) { s += in.peek(); } return s; }
    static int stringEquals(String a, String b, int n) { int s = 0; for (int i = 0; i < n; i++) { if (a.equals(b)) s++; } return s; }
    static int stringCharAt(String a, int n) { int s = 0; for (int i = 0; i < n; i++) { s += a.charAt(i & 7); } return s; }
    static int mathOps(int n) { int s = 0; for (int i = 0; i < n; i++) { s += Math.max(i, 5) + Math.abs(-i) + Math.min(i, 3); } return s; }
    static int listGet(int n) { int s = 0; for (int i = 0; i < n; i++) { s += LIST.get(i & 7); } return s; }
    static int mapGet(Map<String, Integer> m, String[] ks, int n) { int s = 0; for (int i = 0; i < n; i++) { s += m.get(ks[i & 3]); } return s; }
    static int mapPut(Map<Integer, Integer> m, int n) { for (int i = 0; i < n; i++) { m.put(i & 255, i); } return m.size(); }
    static int chmGet(ConcurrentHashMap<String, Integer> m, String[] ks, int n) { int s = 0; for (int i = 0; i < n; i++) { s += m.get(ks[i & 3]); } return s; }
    static int atomicInc(AtomicInteger a, int n) { for (int i = 0; i < n; i++) { a.incrementAndGet(); } return a.get(); }
    static int threadLocal(int n) { int s = 0; for (int i = 0; i < n; i++) { s += TL.get(); } return s; }
    static int getClassName(Object o, int n) { int s = 0; for (int i = 0; i < n; i++) { s += o.getClass().getName().length(); } return s; }
    static int hashCodeCall(Object o, String str, int n) { int s = 0; for (int i = 0; i < n; i++) { s += o.hashCode() + str.hashCode(); } return s; }
    static int optional(int n) { int s = 0; for (int i = 0; i < n; i++) { s += Optional.of(i).map(x -> x + 1).orElse(0); } return s; }
    static int intStream(int n) { int s = 0; for (int i = 0; i < n; i += 10) { s += IntStream.range(0, 10).sum(); } return s; }
    static int streamList(int n) { int s = 0; for (int i = 0; i < n; i += 8) { s += LIST.stream().mapToInt(x -> x).sum(); } return s; }
    static int arrayCopy(int[] a, int[] b, int n) { for (int i = 0; i < n; i++) { System.arraycopy(a, 0, b, 0, 8); } return b[0]; }
    static int newArray2d(int n) { int s = 0; for (int i = 0; i < n; i++) { int[][] m = new int[3][3]; s += m.length; } return s; }
    static int varargs(int n) { int s = 0; for (int i = 0; i < n; i++) { s += Arrays.asList(1, 2, 3).size(); } return s; }
    static int stringFormat(int n) { int s = 0; for (int i = 0; i < n; i++) { s += String.format("%d-%s", i, "x").length(); } return s; }
    static int nanoTime(int n) { long s = 0; for (int i = 0; i < n; i++) { s += System.nanoTime(); } return (int) s; }
    static int currentThread(int n) { int s = 0; for (int i = 0; i < n; i++) { s += Thread.currentThread().hashCode(); } return s; }
    static int instanceofPattern(Object o, int n) { int s = 0; for (int i = 0; i < n; i++) { if (o instanceof Pt p && p.x() > 0) s += p.y(); } return s; }
    static int reflect(java.lang.reflect.Method m, Pt p, int n) throws Exception { int s = 0; for (int i = 0; i < n; i++) { s += (Integer) m.invoke(p); } return s; }
    static int syncMethod(Object lock, int n) { int s = 0; for (int i = 0; i < n; i++) { s += syncCall(); } return s; }
    static synchronized int syncCall() { return 1; }
    static int interfaceDefaultLambda(Function<Integer, Integer> f, int n) { int s = 0; for (int i = 0; i < n; i++) { s += f.apply(i); } return s; }
    static int comparatorSort(int n) { int s = 0; Integer[] a = {5, 3, 9, 1, 7, 2, 8, 4}; for (int i = 0; i < n; i += 8) { Integer[] c = a.clone(); Arrays.sort(c, Comparator.naturalOrder()); s += c[0]; } return s; }

    static long run(String mode, int n) throws Exception {
        String[] keys = {"alpha", "beta", "gamma", "delta"};
        Color[] cs = Color.values();
        Pt p = new Pt(2, 3);
        Shape sh = new Sq(2.0);
        Outer.Inner in = new Outer().new Inner();
        Map<String, Integer> hm = new HashMap<>(); for (String k : keys) hm.put(k, k.length());
        Map<Integer, Integer> hm2 = new HashMap<>();
        ConcurrentHashMap<String, Integer> chm = new ConcurrentHashMap<>(); for (String k : keys) chm.put(k, k.length());
        AtomicInteger ai = new AtomicInteger();
        int[] a8 = new int[8], b8 = new int[8];
        java.lang.reflect.Method meth = Pt.class.getDeclaredMethod("sum"); meth.setAccessible(true);
        Function<Integer, Integer> fn = x -> x + 1;
        long t0 = System.nanoTime();
        int s;
        switch (mode) {
            case "foreachList": s = foreachList(n); break;
            case "foreachArr": s = foreachArr(n); break;
            case "sbAppend": s = sbAppend(n); break;
            case "switchStr": s = switchStr(keys, n); break;
            case "switchEnum": s = switchEnum(cs, n); break;
            case "tryFinally": s = tryFinally(n); break;
            case "boxing": s = boxing(n); break;
            case "boxingBig": s = boxingBig(n); break;
            case "recordAcc": s = recordAcc(p, n); break;
            case "defaultMethod": s = defaultMethod(sh, n); break;
            case "innerAccess": s = innerAccess(in, n); break;
            case "stringEquals": s = stringEquals("hello", new String("hello"), n); break;
            case "stringCharAt": s = stringCharAt("abcdefgh", n); break;
            case "mathOps": s = mathOps(n); break;
            case "listGet": s = listGet(n); break;
            case "mapGet": s = mapGet(hm, keys, n); break;
            case "mapPut": s = mapPut(hm2, n); break;
            case "chmGet": s = chmGet(chm, keys, n); break;
            case "atomicInc": s = atomicInc(ai, n); break;
            case "threadLocal": s = threadLocal(n); break;
            case "getClassName": s = getClassName(p, n); break;
            case "hashCodeCall": s = hashCodeCall(p, "hello", n); break;
            case "optional": s = optional(n); break;
            case "intStream": s = intStream(n); break;
            case "streamList": s = streamList(n); break;
            case "arrayCopy": s = arrayCopy(a8, b8, n); break;
            case "newArray2d": s = newArray2d(n); break;
            case "varargs": s = varargs(n); break;
            case "stringFormat": s = stringFormat(n); break;
            case "nanoTime": s = nanoTime(n); break;
            case "currentThread": s = currentThread(n); break;
            case "instanceofPattern": s = instanceofPattern(p, n); break;
            case "reflect": s = reflect(meth, p, n); break;
            case "syncMethod": s = syncMethod(null, n); break;
            case "lambdaFn": s = interfaceDefaultLambda(fn, n); break;
            case "comparatorSort": s = comparatorSort(n); break;
            default: throw new IllegalArgumentException(mode);
        }
        long t = System.nanoTime() - t0;
        sink += s;
        return t;
    }

    static final String[] ALL = { "foreachList", "foreachArr", "sbAppend", "switchStr", "switchEnum", "tryFinally", "boxing", "boxingBig", "recordAcc", "defaultMethod",
        "innerAccess", "stringEquals", "stringCharAt", "mathOps", "listGet", "mapGet", "mapPut", "chmGet", "atomicInc", "threadLocal", "getClassName", "hashCodeCall",
        "optional", "intStream", "streamList", "arrayCopy", "newArray2d", "varargs", "stringFormat", "nanoTime", "currentThread", "instanceofPattern", "reflect",
        "syncMethod", "lambdaFn", "comparatorSort" };

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "all";
        int n = args.length > 1 ? Integer.parseInt(args[1]) : 200_000;
        String[] modes = mode.equals("all") ? ALL : new String[] { mode };
        for (String m : modes) {
            int nn = (m.equals("stringFormat") || m.equals("optional") || m.equals("reflect") || m.equals("streamList") || m.equals("comparatorSort")) ? Math.max(n / 20, 1000) : n;
            run(m, Math.min(nn, 2000));
            long best = Long.MAX_VALUE;
            for (int r = 0; r < 5; r++) best = Math.min(best, run(m, nn));
            System.out.println(m + " " + String.format("%.1f", best / (double) nn));
        }
        if (sink == 42) System.out.println("x");
    }
}
