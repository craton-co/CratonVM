import com.sun.management.ThreadMXBean;
import java.lang.management.ManagementFactory;
import java.util.*;

/**
 * Per-shape allocation cost, measured the same way HqlParserMemoryUsageTest
 * measures its budget: com.sun.management.ThreadMXBean.getTotalThreadAllocatedBytes().
 *
 * Run under HotSpot and CratonVM and read the ratio column. A shape that costs
 * several times more on one VM names a mechanism; a uniform ratio names none.
 *
 * Each shape is run in its own method with a sink so nothing is trivially dead,
 * and every loop body is measured after an unmeasured warm pass.
 */
public class AllocShapeProbe {
    static final ThreadMXBean TMX = (ThreadMXBean) ManagementFactory.getThreadMXBean();
    static Object sink;
    static long sinkLong;

    static long bytes() {
        return TMX.getCurrentThreadAllocatedBytes();
    }

    interface Shape { void run(int n); }

    static void measure(String name, int n, Shape s) {
        s.run(Math.min(n, 200));           // warm, unmeasured
        long a = bytes();
        s.run(n);
        long b = bytes();
        double per = (b - a) / (double) n;
        System.out.println(String.format("%-34s %10.1f bytes/op", name, per));
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 200000;

        measure("HashMap empty", n, k -> { for (int i=0;i<k;i++) sink = new HashMap<String,String>(); });
        measure("HashMap 1 entry", n, k -> { for (int i=0;i<k;i++) { HashMap<String,String> m=new HashMap<>(); m.put("a","b"); sink=m; } });
        measure("HashMap 4 entries", n, k -> { for (int i=0;i<k;i++) { HashMap<Integer,Integer> m=new HashMap<>(); for(int j=0;j<4;j++) m.put(j,j); sink=m; } });
        measure("HashSet empty", n, k -> { for (int i=0;i<k;i++) sink = new HashSet<String>(); });
        measure("HashSet 4 entries", n, k -> { for (int i=0;i<k;i++) { HashSet<Integer> s2=new HashSet<>(); for(int j=0;j<4;j++) s2.add(j); sink=s2; } });
        measure("LinkedHashMap empty", n, k -> { for (int i=0;i<k;i++) sink = new LinkedHashMap<String,String>(); });
        measure("IdentityHashMap empty", n, k -> { for (int i=0;i<k;i++) sink = new IdentityHashMap<String,String>(); });
        measure("ConcurrentHashMap empty", n/10, k -> { for (int i=0;i<k;i++) sink = new java.util.concurrent.ConcurrentHashMap<String,String>(); });
        measure("ArrayList empty", n, k -> { for (int i=0;i<k;i++) sink = new ArrayList<String>(); });
        measure("ArrayList 4 adds", n, k -> { for (int i=0;i<k;i++) { ArrayList<Integer> l=new ArrayList<>(); for(int j=0;j<4;j++) l.add(j); sink=l; } });
        measure("Object[4]", n, k -> { for (int i=0;i<k;i++) sink = new Object[4]; });
        measure("int[8]", n, k -> { for (int i=0;i<k;i++) sink = new int[8]; });
        measure("plain Object", n, k -> { for (int i=0;i<k;i++) sink = new Object(); });
        measure("BitSet(64)", n, k -> { for (int i=0;i<k;i++) sink = new BitSet(64); });
        measure("Integer.valueOf(small)", n, k -> { for (int i=0;i<k;i++) sink = Integer.valueOf(i & 63); });
        measure("Integer.valueOf(large)", n, k -> { for (int i=0;i<k;i++) sink = Integer.valueOf(100000 + i); });
        measure("StringBuilder + 3 appends", n, k -> { for (int i=0;i<k;i++) { StringBuilder sb=new StringBuilder(); sb.append("ab").append(i).append('c'); sink=sb.toString(); } });
        measure("String.substring", n, k -> { String s2="abcdefghijklmnop"; for (int i=0;i<k;i++) sink = s2.substring(i & 7); });
        measure("iterator over 4-list", n, k -> { ArrayList<Integer> l=new ArrayList<>(List.of(1,2,3,4)); for (int i=0;i<k;i++) { long t=0; for (Integer v : l) t+=v; sinkLong=t; } });
        measure("map.entrySet iteration", n, k -> { HashMap<Integer,Integer> m=new HashMap<>(); for(int j=0;j<4;j++) m.put(j,j); for (int i=0;i<k;i++) { long t=0; for (Map.Entry<Integer,Integer> e : m.entrySet()) t+=e.getValue(); sinkLong=t; } });
        measure("Arrays.asList(4).hashCode", n, k -> { Object[] a4={1,2,3,4}; for (int i=0;i<k;i++) sinkLong = Arrays.asList(a4).hashCode(); });
        measure("lambda capture + call", n, k -> { for (int i=0;i<k;i++) { int c=i; Runnable r=() -> sinkLong=c; r.run(); } });
        measure("boxed HashMap.get miss", n, k -> { HashMap<Integer,Integer> m=new HashMap<>(); for(int j=0;j<8;j++) m.put(j,j); for (int i=0;i<k;i++) sink = m.get(1000+i); });
        System.out.println("SHAPE_END");
    }
}
