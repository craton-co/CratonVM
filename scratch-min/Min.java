// Minimal reproducer attempt for A2 (ReflRepro.scan miscompile shape):
//  - a StringBuilder held across an Object[] for-each loop
//  - a call inside the loop to a helper that BAILS JIT (string '+' => invokedynamic)
//    so it runs interpreted and allocates heavily (frequent young GC under stress)
public class Min {
    // String '+' compiles to invokedynamic makeConcatWithConstants => JIT bails => interpreted + heavy alloc
    static String helper(String x) {
        return "v" + x + "-" + x.length() + "-" + Integer.toString(x.length(), 16) + "#" + x.hashCode();
    }
    static String scan(String[] arr) {
        StringBuilder sb = new StringBuilder();
        for (String s : arr) {
            if (s == null) continue;
            sb.append(helper(s)).append(';');
        }
        return sb.toString();
    }
    public static void main(String[] a) {
        int n = a.length > 0 ? Integer.parseInt(a[0]) : 200000;
        String[] arr = new String[8];
        for (int i = 0; i < 8; i++) arr[i] = "field" + i;
        String ref = scan(arr);
        long ok = 0, bad = 0; String fb = null;
        for (int i = 0; i < n; i++) {
            String r;
            try { r = scan(arr); }
            catch (Throwable t) { bad++; if (fb == null) fb = t.getClass().getName() + ":" + t.getMessage(); continue; }
            if (r.equals(ref)) ok++; else { bad++; if (fb == null) fb = "MISMATCH:" + r; }
        }
        System.out.println("done ok=" + ok + " bad=" + bad + (fb != null ? " fb=" + fb : ""));
    }
}
