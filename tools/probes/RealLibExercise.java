import com.fasterxml.jackson.databind.ObjectMapper;
import org.apache.commons.lang3.StringUtils;
import java.util.*;

/** A deterministic exercise of two REAL third-party libraries — Jackson's
 *  databind round-trip and commons-lang3's string helpers — chosen because
 *  they allocate hard and print a checksum rather than a timing. */
public class RealLibExercise {
    public static class Row {
        public int id; public String name; public List<Integer> tags = new ArrayList<>();
        public Row() {}
        public Row(int id, String name) { this.id = id; this.name = name;
            for (int i = 0; i < (id % 7) + 1; i++) tags.add(id * 31 + i); }
    }
    public static void main(String[] a) throws Exception {
        int n = a.length > 0 ? Integer.parseInt(a[0]) : 20000;
        ObjectMapper m = new ObjectMapper();
        long sum = 0;
        for (int i = 0; i < n; i++) {
            Row r = new Row(i, StringUtils.repeat("ab", (i % 5) + 1) + i);
            String json = m.writeValueAsString(r);
            Row back = m.readValue(json, Row.class);
            sum += back.id + back.name.length() + back.tags.size()
                 + StringUtils.countMatches(json, "\"") ;
        }
        Map<String, Integer> counts = new TreeMap<>();
        for (int i = 0; i < n; i++) counts.merge(StringUtils.left("k" + (i % 97), 3), 1, Integer::sum);
        long cs = 0; for (Map.Entry<String,Integer> e : counts.entrySet()) cs += e.getKey().hashCode() * e.getValue();
        System.out.println("rows=" + n + " sum=" + sum + " keys=" + counts.size() + " cs=" + cs);
        System.out.println("PASS RealLibExercise");
    }
}
