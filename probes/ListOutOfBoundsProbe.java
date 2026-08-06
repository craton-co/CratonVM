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

        System.out.println("List.of size 3 -- ListN boundary");
        List<String> three = List.of("a", "b", "c");
        t("three.get(3)       ", () -> three.get(3));
        t("three.get(-1)      ", () -> three.get(-1));
        t("List.copyOf size 2 ", () -> List.copyOf(Arrays.asList("a", "b")).get(2));
        t("List.copyOf size 3 ", () -> List.copyOf(Arrays.asList("a", "b", "c")).get(3));

        System.out.println("Collections.unmodifiableList -- delegates to its backing");
        List<String> uOne = java.util.Collections.unmodifiableList(
                new ArrayList<>(List.of("a")));
        t("unmod(ArrayList 1).get(3)", () -> uOne.get(3));
        t("unmod(ArrayList 1).get(-1)", () -> uOne.get(-1));
        List<String> uTwo = java.util.Collections.unmodifiableList(
                new ArrayList<>(List.of("a", "b")));
        t("unmod(ArrayList 2).get(5)", () -> uTwo.get(5));
        List<String> uArrays = java.util.Collections.unmodifiableList(
                Arrays.asList("a", "b"));
        t("unmod(Arrays.asList).get(5)", () -> uArrays.get(5));
        List<String> uLinked = java.util.Collections.unmodifiableList(
                new java.util.LinkedList<>(List.of("a", "b")));
        t("unmod(LinkedList).get(5)", () -> uLinked.get(5));
        t("unmod(LinkedList).get(0) ok", () -> uLinked.get(0));
        List<String> uEmpty = java.util.Collections.unmodifiableList(new ArrayList<>());
        t("unmod(empty).get(0)", () -> uEmpty.get(0));

        System.out.println("LinkedList direct");
        List<String> linked = new java.util.LinkedList<>(List.of("a", "b"));
        t("linked.get(5)      ", () -> linked.get(5));
        t("linked.get(-1)     ", () -> linked.get(-1));

        System.out.println("negative indices");
        t("Arrays.asList.get(-1)", () -> al.get(-1));
        t("ArrayList.get(-1)  ", () -> arr.get(-1));
        t("sub.get(-1)        ", () -> sub.get(-1));

        // The concrete classes. This VM funnels `List.of` and
        // `Collections.unmodifiableList` through one synthetic class, but it
        // already reports these two names apart -- so whatever tells them
        // apart for `getClass()` is available to the throw site as well.
        System.out.println("LinkedList positional mutators");
        t("linked.set(5,\"z\")   ", () -> new java.util.LinkedList<>(List.of("a", "b")).set(5, "z"));
        t("linked.add(9,\"z\")   ", () -> {
            new java.util.LinkedList<>(List.of("a", "b")).add(9, "z");
            return "ok";
        });
        t("linked.remove(9)     ", () -> new java.util.LinkedList<>(List.of("a", "b")).remove(9));
        t("linked.add(2,\"z\") ok", () -> {
            java.util.LinkedList<String> l = new java.util.LinkedList<>(List.of("a", "b"));
            l.add(2, "z");
            return l.toString();
        });

        System.out.println("singleton / empty wrappers");
        List<String> single = java.util.Collections.singletonList("a");
        t("singletonList.get(1) ", () -> single.get(1));
        t("singletonList.get(-1)", () -> single.get(-1));
        t("singletonList.get(0) ", () -> single.get(0));
        List<String> emptyL = java.util.Collections.emptyList();
        t("emptyList.get(0)     ", () -> emptyL.get(0));

        System.out.println("listIterator(int)");
        t("List.of(a,b).listIterator(5)", () -> two.listIterator(5));
        t("List.of(a,b).listIterator(-1)", () -> two.listIterator(-1));
        t("ArrayList.listIterator(5)", () -> arr.listIterator(5));
        t("Arrays.asList.subList(0,5)", () -> al.subList(0, 5));
        t("ArrayList.subList(0,5)", () -> arr.subList(0, 5));
        t("ArrayList.subList(-1,1)", () -> arr.subList(-1, 1));
        t("ArrayList.subList(2,1)", () -> arr.subList(2, 1));

        System.out.println("concrete classes");
        System.out.println("  List.of(a).getClass       = " + one.getClass().getName());
        System.out.println("  List.of(a,b,c,d).getClass = " + n.getClass().getName());
        System.out.println("  List.of().getClass        = " + List.of().getClass().getName());
        System.out.println("  Arrays.asList.getClass    = " + al.getClass().getName());
        System.out.println("  unmod(ArrayList).getClass = " + uOne.getClass().getName());
        System.out.println("  unmod(LinkedList).getClass= " + uLinked.getClass().getName());
        System.out.println("  ArrayList.getClass        = " + arr.getClass().getName());
        System.out.println("  subList.getClass          = " + sub.getClass().getName());

        System.out.println("LIST-OOB-PROBE-DONE");
    }
}
