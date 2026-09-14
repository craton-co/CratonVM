import java.util.*; import java.util.concurrent.*;
public class ChmOrderConfoundOrderTest {
    static void plain(String tag) {
        ConcurrentHashMap<String,String> m = new ConcurrentHashMap<>();
        for (int i=0;i<4;i++) m.put("k"+i,"v"+i);
        System.out.println(tag+" back-to-back      size="+m.size()+" keys="+new TreeSet<>(m.keySet()));
    }
    static void withAbs(String tag) {
        ConcurrentHashMap<String,String> m = new ConcurrentHashMap<>();
        for (int i=0;i<4;i++){ m.put("k"+i,"v"+i); Math.abs(1); }
        System.out.println(tag+" with Math.abs(1)  size="+m.size()+" keys="+new TreeSet<>(m.keySet()));
    }
    public static void main(String[] a) {
        if (a.length>0 && a[0].equals("absfirst")) { withAbs("[1st]"); plain("[2nd]"); }
        else { plain("[1st]"); withAbs("[2nd]"); }
    }
}
