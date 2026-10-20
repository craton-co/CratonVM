import java.util.ArrayList;
import java.util.Collections;
import java.util.List;
import java.util.Map;

public class CollectionsSyntheticProbe {
    static void row(String name, Runnable r) {
        try {
            r.run();
            System.out.println("R " + name + " = no-throw");
        } catch (Throwable t) {
            System.out.println("R " + name + " = " + t.getClass().getName() + ": " + t.getMessage());
        }
    }

    public static void main(String[] args) {
        System.out.println("R emptyList.cls = " + Collections.emptyList().getClass().getName());
        row("emptyList.add", () -> Collections.emptyList().add("b"));

        System.out.println("R singletonList.cls = " + Collections.singletonList("a").getClass().getName());
        row("singletonList.add", () -> Collections.singletonList("a").add("b"));

        System.out.println("R singletonMap.cls = " + Collections.singletonMap("k", "v").getClass().getName());
        row("singletonMap.put", () -> Collections.singletonMap("k", "v").put("x", "y"));

        List<String> checked = Collections.checkedList(new ArrayList<String>(), String.class);
        System.out.println("R checkedList.cls = " + checked.getClass().getName());
        row("checkedList.addBadType", () -> {
            @SuppressWarnings({"unchecked", "rawtypes"})
            List raw = checked;
            raw.add(Integer.valueOf(1));
        });

        List<Integer> dst3 = new ArrayList<>(List.of(7, 8, 9));
        List<Integer> src2 = new ArrayList<>(List.of(4, 5));
        Collections.copy(dst3, src2);
        System.out.println("R copy.unequalLen dst=" + dst3);

        List<Integer> dst2 = new ArrayList<>(List.of(1, 2));
        List<Integer> src2b = new ArrayList<>(List.of(4, 5));
        Collections.copy(dst2, src2b);
        System.out.println("R copy.equalLen dst=" + dst2);

        row("copy.shortDst", () -> {
            List<Integer> shortDst = new ArrayList<>(List.of(0));
            List<Integer> longSrc = new ArrayList<>(List.of(1, 2, 3));
            Collections.copy(shortDst, longSrc);
        });

        row("synchronizedList.add", () -> {
            List<String> sl = Collections.synchronizedList(new ArrayList<>());
            sl.add("x");
            System.out.println("R synchronizedList.add.size=" + sl.size());
        });

        row("singleton.set.add", () -> {
            java.util.Set<String> ss = Collections.singleton("a");
            System.out.println("R singleton.set.cls=" + ss.getClass().getName());
            ss.add("b");
        });

        row("collections.enumeration", () -> {
            java.util.Enumeration<Integer> e = Collections.enumeration(List.of(1, 2, 3));
            int c = 0;
            while (e.hasMoreElements()) { e.nextElement(); c++; }
            System.out.println("R collections.enumeration.count=" + c);
        });

        System.out.println("PASS CollectionsSyntheticProbe");
    }
}
