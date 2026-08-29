import java.util.*;

/** Which class does each of the five receivers' iterator actually have?
 *
 *  The method-reference door record explains the three ISE rows by "the
 *  iterator's class NAME is real while the INSTANCE is minted by this VM".
 *  That is a claim about a class name, and this prints it rather than
 *  assuming it. Run on HotSpot too: the two must agree for the explanation
 *  to hold, and where they do not, the door is not the whole story. */
public class ItrClassNameProbe {
    static int rows = 0;

    static void p(String tag, Object v) {
        rows++;
        System.out.println(rows + " " + tag + " |" + v + "|");
    }

    public static void main(String[] a) {
        List<String> al = new ArrayList<>(List.of("a", "b", "c"));
        p("ArrayList itr", al.iterator().getClass().getName());

        Set<String> hs = new HashSet<>(Set.of("a", "b", "c"));
        p("HashSet itr", hs.iterator().getClass().getName());

        Map<String, String> hm = new HashMap<>(Map.of("k", "v", "k2", "v2"));
        p("HashMap keySet itr", hm.keySet().iterator().getClass().getName());

        Hashtable<String, String> ht = new Hashtable<>();
        ht.put("k", "v");
        ht.put("k2", "v2");
        p("Hashtable keySet itr", ht.keySet().iterator().getClass().getName());

        Properties pr = new Properties();
        pr.setProperty("k", "v");
        pr.setProperty("k2", "v2");
        p("Properties keySet itr", pr.keySet().iterator().getClass().getName());

        // And the interface the bound reference resolves through.
        Iterator<String> it = hs.iterator();
        p("HashSet itr is Iterator", (it instanceof Iterator));

        System.out.println("DONE ItrClassNameProbe");
    }
}
