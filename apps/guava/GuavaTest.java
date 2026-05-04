import com.google.common.collect.ImmutableList;
import com.google.common.collect.ImmutableMap;
import com.google.common.base.Joiner;

public class GuavaTest {
    public static void main(String[] args) {
        ImmutableList<String> xs = ImmutableList.of("a", "b", "c");
        System.out.println("xs=" + xs);

        ImmutableMap<String, Integer> m = ImmutableMap.of("one", 1, "two", 2);
        System.out.println("m.one=" + m.get("one") + " m.two=" + m.get("two"));

        String joined = Joiner.on(",").join(xs);
        System.out.println("joined=" + joined);
        if (!"a,b,c".equals(joined)) throw new AssertionError("join failed");
        System.out.println("GuavaTest: PASS");
    }
}
