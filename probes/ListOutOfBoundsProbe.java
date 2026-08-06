import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;

/**
 * Out-of-range `List.get` / `set` / `add`: exception CLASS and MESSAGE.
 *
 * `List.of("a").get(3)` throws `ArrayIndexOutOfBoundsException` here where
 * HotSpot throws plain `IndexOutOfBoundsException`. AIOOBE is a *subclass*, so
 * `catch (IndexOutOfBoundsException)` still catches it but a class test does
 * not -- the direction that breaks a `catch`.
 *
 * The implementations diverge internally and must be probed separately:
 *
 *   * `List.of()` with 1-2 elements is `ImmutableCollections.List12`,
 *   * with 3+ it is `ListN` -- different bounds code,
 *   * `Arrays.asList` is `Arrays$ArrayList` and indexes a real array,
 *   * `ArrayList` has its own `Objects.checkIndex` path,
 *   * `subList` adds an offset, which is where an off-by-one hides.
 *
 * HotSpot's messages differ per implementation too ("Index: 3 Size: 1" vs
 * "Index 3 out of bounds for length 1"), so the message is only meaningful
 * against the control -- print it, do not assume a single format.
 */
public class ListOutOfBoundsProbe {

    static void t(String label, java.util.function.Supplier<Object> f) {
        try {
            System.out.println("  " + label + " => NO-THROW value=" + f.get());
        } catch (Throwable e) {
            String m = e.getMessage();
            System.out.println("  " + label + " => " + e.getClass().getName()
                    + " msg=" + m);
        }
    }

    public static void main(String[] args) {
        System.out.println("List.of(\"a\")  -- List12, size 1");
        List<String> one = List.of("a");
        t("get(0)  in range   ", () -> one.get(0));
        t("get(1)  one past   ", () -> one.get(1));
        t("get(3)             ", () -> one.get(3));
        t("get(-1)            ", () -> one.get(-1));

        System.out.println("List.of(\"a\",\"b\")  -- List12, size 2");
        List<String> two = List.of("a", "b");
        t("get(2)             ", () -> two.get(2));

        System.out.println("List.of(a,b,c,d)  -- ListN, size 4");
        List<String> n = List.of("a", "b", "c", "d");
        t("get(4)             ", () -> n.get(4));
        t("get(99)            ", () -> n.get(99));

        System.out.println("Arrays.asList(a,b)");
        List<String> al = Arrays.asList("a", "b");
        t("get(5)             ", () -> al.get(5));
        t("set(5,\"z\")         ", () -> al.set(5, "z"));

        System.out.println("ArrayList size 2");
        List<String> arr = new ArrayList<>(Arrays.asList("a", "b"));
        t("get(5)             ", () -> arr.get(5));
        t("set(5,\"z\")         ", () -> arr.set(5, "z"));
        t("add(9,\"z\")         ", () -> { arr.add(9, "z"); return "ok"; });
        t("remove(9)          ", () -> arr.remove(9));

        System.out.println("subList(1,2) of ArrayList size 2");
        List<String> sub = arr.subList(1, 2);
        t("sub.get(0)         ", () -> sub.get(0));
        t("sub.get(1) past    ", () -> sub.get(1));
        t("sub.get(5)         ", () -> sub.get(5));

        System.out.println("empty");
        t("List.of().get(0)   ", () -> List.of().get(0));
        t("new ArrayList.get(0)", () -> new ArrayList<>().get(0));

        System.out.println("LIST-OOB-PROBE-DONE");
    }
}
