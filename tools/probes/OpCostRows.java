// Coarse per-operation cost rows: one kernel per row, each a compiled method
// with its own loop, called many times (so the timed body is a normal compiled
// entry, not an on-stack-replacement body). Prints "row ns/op".
//
//   java OpCostRows            all rows, timing
//   java OpCostRows count <row> <ops>   run exactly <ops> operations of one row
//       after warm-up; difference two op counts under an instruction counter
//       ((I(3N) - I(N)) / 2N) to get instructions per operation.
import java.util.*;
import java.util.function.IntUnaryOperator;

public class OpCostRows {
    static final int N = 10_000;
    static final class P { int x; P next; Object ref; P(int x) { this.x = x; } }
    interface Shape { int area(int i); }
    static final class Sq implements Shape { public int area(int i) { return i + 1; } }
    static final class Ci implements Shape { public int area(int i) { return i + 2; } }
    static final class Tr implements Shape { public int area(int i) { return i + 3; } }
    static final class Re implements Shape { public int area(int i) { return i + 4; } }

    static final P RING; static final P HOLD = new P(0); static final P[] PS = new P[64];
    static final Object[] OBJS = new Object[8]; static final Object[] DST = new Object[64];
    static final Shape[] TWO = { new Sq(), new Ci() };
    static final Shape[] FOUR = { new Sq(), new Ci(), new Tr(), new Re() };
    static final HashMap<Integer, Integer> MAP = new HashMap<>();
    static final Integer[] KEYS = new Integer[1024];
    static final ArrayList<Integer> LIST = new ArrayList<>();
    static final String STR = "the quick brown fox jumps over the lazy dog, twice over and again";
    static final Object LOCK = new Object(); static int counter;
    static final int[] SRC = new int[16], DSTI = new int[16];
    static final IntUnaryOperator INC = v -> v + 1;
    static final RuntimeException EX = new RuntimeException("x");
    static {
        P head = new P(0), p = head;
        for (int i = 1; i < 64; i++) { p.next = new P(i); p = p.next; }
        p.next = head; RING = head;
        for (int i = 0; i < 8; i++) OBJS[i] = new P(i);
        for (int i = 0; i < 64; i++) PS[i] = new P(i);
        for (int i = 0; i < 1024; i++) { KEYS[i] = i * 7 + 1000; MAP.put(KEYS[i], i); LIST.add(i + 1000); }
    }

    static long arith(int n) { long s = 1; for (int i = 0; i < n; i++) s = s * 31 + i; return s; }
    static long getfield(int n) { long s = 0; P p = RING; for (int i = 0; i < n; i++) { s += p.x; p = p.next; } return s; }
    static long putInt(int n) { P p = HOLD; for (int i = 0; i < n; i++) p.x += i; return p.x; }
    static long putRef(int n) { P[] ps = PS; Object[] o = OBJS; for (int i = 0; i < n; i++) ps[i & 63].ref = o[i & 7]; return ps[n & 63].ref == null ? 0 : n; }
    static long aastore(int n) { Object[] d = DST, o = OBJS; for (int i = 0; i < n; i++) d[i & 63] = o[i & 7]; return n; }
    static long virt2(int n) { long s = 0; Shape[] t = TWO; for (int i = 0; i < n; i++) s += t[i & 1].area(i); return s; }
    static long iface4(int n) { long s = 0; Shape[] t = FOUR; for (int i = 0; i < n; i++) s += t[i & 3].area(i); return s; }
    static long lambda(int n) { long s = 0; IntUnaryOperator f = INC; for (int i = 0; i < n; i++) s += f.applyAsInt(i); return s; }
    static long allocObj(int n) { Object[] d = DST; for (int i = 0; i < n; i++) d[i & 63] = new P(i); return n; }
    static long allocArr(int n) { Object[] d = DST; for (int i = 0; i < n; i++) d[i & 63] = new int[4]; return n; }
    static long box(int n) { long s = 0; for (int i = 0; i < n; i++) { Integer b = i + 1000; s += b.hashCode(); } return s; }
    static long mapGet(int n) { long s = 0; Integer[] k = KEYS; for (int i = 0; i < n; i++) s += MAP.get(k[i & 1023]); return s; }
    static long listGet(int n) { long s = 0; for (int i = 0; i < n; i++) s += LIST.get(i & 1023); return s; }
    static long charAt(int n) { long s = 0; String t = STR; for (int i = 0; i < n; i++) s += t.charAt(i & 63); return s; }
    static long sbAppend(int n) { StringBuilder sb = new StringBuilder(64); long s = 0; for (int i = 0; i < n; i++) { sb.append((char) ('a' + (i & 15))); if (sb.length() == 64) { s += sb.length(); sb.setLength(0); } } return s; }
    static long sync(int n) { for (int i = 0; i < n; i++) { synchronized (LOCK) { counter++; } } return counter; }
    static long typeCheck(int n) { long s = 0; Object[] o = OBJS; Shape[] t = FOUR; for (int i = 0; i < n; i++) { Object v = (i & 1) == 0 ? o[i & 7] : t[i & 3]; if (v instanceof P) s += ((P) v).x; else s += 1; } return s; }
    static long arraycopy(int n) { for (int i = 0; i < n; i++) System.arraycopy(SRC, 0, DSTI, 0, 16); return n; }
    static long throwCatch(int n) { long s = 0; for (int i = 0; i < n / 100; i++) { try { if (i >= 0) throw EX; } catch (RuntimeException e) { s++; } } return s; }
    static long throwNew(int n) { long s = 0; for (int i = 0; i < n / 100; i++) { try { if (i >= 0) throw new IllegalStateException(); } catch (IllegalStateException e) { s++; } } return s; }

    static final String[] ROWS = { "arith", "getfield", "putInt", "putRef", "aastore", "virt2", "iface4", "lambda",
        "allocObj", "allocArr", "box", "mapGet", "listGet", "charAt", "sbAppend", "sync", "typeCheck", "arraycopy",
        "throwCatch", "throwNew" };

    static long run(String row, int n) {
        switch (row) {
            case "arith": return arith(n); case "getfield": return getfield(n); case "putInt": return putInt(n);
            case "putRef": return putRef(n); case "aastore": return aastore(n); case "virt2": return virt2(n);
            case "iface4": return iface4(n); case "lambda": return lambda(n); case "allocObj": return allocObj(n);
            case "allocArr": return allocArr(n); case "box": return box(n); case "mapGet": return mapGet(n);
            case "listGet": return listGet(n); case "charAt": return charAt(n); case "sbAppend": return sbAppend(n);
            case "sync": return sync(n); case "typeCheck": return typeCheck(n); case "arraycopy": return arraycopy(n);
            case "throwCatch": return throwCatch(n); case "throwNew": return throwNew(n);
            default: throw new IllegalArgumentException(row);
        }
    }

    static long sink;
    static long spin(String row, long calls) { long s = 0; for (long c = 0; c < calls; c++) s += run(row, N); return s; }

    public static void main(String[] a) {
        if (a.length == 3 && a[0].equals("count")) {
            long perCall = a[1].startsWith("throw") ? (N / 100) : N;
            sink += spin(a[1], 3000);
            sink += spin(a[1], Long.parseLong(a[2]) / perCall);
            System.out.println("done " + sink);
            return;
        }
        for (String row : ROWS) {
            sink += spin(row, 3000);
            long best = Long.MAX_VALUE;
            for (int rep = 0; rep < 5; rep++) {
                long t0 = System.nanoTime();
                sink += spin(row, 1000);
                best = Math.min(best, System.nanoTime() - t0);
            }
            double ops = 1000.0 * (row.startsWith("throw") ? N / 100 : N);
            System.out.printf(Locale.ROOT, "%s %.2f%n", row, best / ops);
        }
        System.out.println("done " + sink);
    }
}
