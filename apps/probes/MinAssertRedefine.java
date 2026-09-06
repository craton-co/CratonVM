import java.util.*;
import org.assertj.core.internal.StandardComparisonStrategy;

public class MinAssertRedefine {
    public static abstract class Foo { public abstract String go(); public String other() { return "x"; } }
    static int diverged = 0;
    static void row(String tag, Object a, Object b) {
        boolean plain = a.equals(b);
        boolean scs = StandardComparisonStrategy.instance().areEqual(a, b);
        boolean util = org.assertj.core.util.Objects.areEqual(a, b);
        boolean bad = !(plain == scs && plain == util);
        if (bad) diverged++;
        System.out.printf("%-40s plain=%-5s scs=%-5s util=%-5s%s%n", tag, plain, scs, util, bad ? "  <<< DIVERGES" : "");
    }
    static void suite(String phase) {
        System.out.println("---- " + phase + " ----");
        List<String> single = Collections.singletonList("test.member");
        List<String> al = new ArrayList<>(single);
        List<String> unmod = Collections.unmodifiableList(al);
        row("unmod vs single", unmod, single);
        row("al vs single", al, single);
        row("listOf vs single", List.of("test.member"), single);
        row("unmodSet vs Set.of", Collections.unmodifiableSet(new LinkedHashSet<>(Arrays.asList("a","b"))), Set.of("a","b"));
        Map<String,Object> m1 = new LinkedHashMap<>(); m1.put("k","v");
        row("map vs Map.of", m1, Map.of("k","v"));
        row("string vs string", new String("abc"), "abc");
    }
    public static void main(String[] x) throws Exception {
        suite("before mock");
        if (x.length == 0 || !x[0].equals("nomock")) {
            Foo f = org.mockito.Mockito.mock(Foo.class);
            System.out.println("mock created: " + f.getClass().getName());
        }
        suite("after mock");
        System.out.println(diverged == 0 ? "PROBE-OK" : "PROBE-FAIL diverged=" + diverged);
    }
}
