import org.apache.commons.lang3.StringUtils;
import org.apache.commons.lang3.RandomStringUtils;
import org.apache.commons.lang3.tuple.Pair;

public class CommonsLang3Probe {
    public static void main(String[] args) {
        String reversed = StringUtils.reverse("hello");
        System.out.println("reversed=" + reversed);
        if (!"olleh".equals(reversed)) throw new AssertionError("reverse failed");

        String rnd = RandomStringUtils.randomAlphanumeric(8);
        System.out.println("rnd.len=" + rnd.length());

        Pair<String, Integer> p = Pair.of("answer", 42);
        System.out.println("pair=" + p.getLeft() + "/" + p.getRight());
        System.out.println("CommonsLang3Probe: PASS");
    }
}
