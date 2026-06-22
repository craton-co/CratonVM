// A2 narrowing: does a FRESH array allocated INSIDE scan each call (vs a param array)
// reproduce? Min (param array) did NOT. Min2 allocates the array inside scan via a
// Java helper (not native). Two loops, like ReflRepro.scan.
public class Min2 {
    static String[] makeArr(int seed) {
        String[] a = new String[6];
        for (int i = 0; i < 6; i++) a[i] = "fld" + (seed + i);
        return a;
    }
    static String helper(String x) {
        return "v" + x + "-" + x.length() + "-" + Integer.toString(x.length(), 16) + "#" + x.hashCode();
    }
    static String scan(int seed) {
        StringBuilder sb = new StringBuilder();
        for (String s : makeArr(seed)) {          // fresh array allocated inside scan
            if (s == null) continue;
            sb.append(helper(s)).append(';');
        }
        for (String s : makeArr(seed + 100)) {     // second loop, second fresh array
            if (s == null) continue;
            sb.append(s).append('(').append(s.length()).append(')').append(';');
        }
        return sb.toString();
    }
    public static void main(String[] a) {
        int n = a.length > 0 ? Integer.parseInt(a[0]) : 200000;
        String ref = scan(0);
        long ok = 0, bad = 0; String fb = null;
        for (int i = 0; i < n; i++) {
            String r;
            try { r = scan(0); }
            catch (Throwable t) { bad++; if (fb == null) fb = t.getClass().getName() + ":" + t.getMessage(); continue; }
            if (r.equals(ref)) ok++; else { bad++; if (fb == null) fb = "MISMATCH:" + r; }
        }
        System.out.println("done ok=" + ok + " bad=" + bad + (fb != null ? " fb=" + fb : ""));
    }
}
