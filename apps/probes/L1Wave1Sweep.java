import java.util.*;
import java.util.stream.Collectors;

/** L1 wave 1 — the rows no probe in the tree dispatched.
 *
 *  Written for precondition 4: every row below is a triple this lane proposes
 *  to retire that had `invocations = 0` across the whole 117-probe union, so
 *  retiring it would have been a change no instrument could see. Each line is
 *  chosen so the EMPTY answer and the right answer print differently — a
 *  `size()` that answers 0 and a `toArray()` that answers `[]` are the two
 *  shapes a broken retirement produces, and both are visible here.
 *
 *  Prints no identity hash, no address and no iteration order of a hash
 *  container: the `SetFromMap` rows sort their output.
 */
public final class L1Wave1Sweep {
    static int rows = 0;
    static void t(String tag, Object v) { rows++; System.out.println(tag + " |" + v + "|"); }
    static void thrown(String tag, Runnable r) {
        rows++;
        try { r.run(); System.out.println(tag + " |no-throw|"); }
        catch (Throwable e) {
            String m = e.getMessage();
            System.out.println(tag + " |" + e.getClass().getName() + (m == null ? "" : ": " + m) + "|");
        }
    }

    static List<String> base() { return new ArrayList<>(List.of("a", "b", "c", "d", "e", "f")); }

    public static void main(String[] a) {
        // --- java/util/ArrayList.toArray(IntFunction) ------------------------
        List<String> al = base();
        t("al.toArrayIntFn", Arrays.toString(al.toArray(String[]::new)));
        t("al.toArrayIntFn.type", al.toArray(String[]::new).getClass().getName());

        // --- java/util/ArrayList$SubList -------------------------------------
        List<String> sub = base().subList(1, 5);            // [b, c, d, e]
        t("sub.isEmpty", sub.isEmpty());
        t("sub.isEmpty.empty", base().subList(2, 2).isEmpty());
        t("sub.getFirst", sub.getFirst());
        t("sub.getLast", sub.getLast());
        t("sub.containsAll.yes", sub.containsAll(List.of("b", "e")));
        t("sub.containsAll.no", sub.containsAll(List.of("b", "z")));
        t("sub.containsAll.empty", sub.containsAll(List.of()));
        StringBuilder fe = new StringBuilder();
        sub.forEach(s -> fe.append(s).append('.'));
        t("sub.forEach", fe.toString());
        t("sub.toArrayIntFn", Arrays.toString(sub.toArray(String[]::new)));
        t("sub.stream", sub.stream().collect(Collectors.joining("-")));
        t("sub.parallelStream", sub.parallelStream().sorted().collect(Collectors.joining("-")));
        t("sub.reversed", sub.reversed().toString());
        t("sub.reversed.size", sub.reversed().size());

        List<String> owner = base();
        List<String> m = owner.subList(1, 4);               // [b, c, d]
        m.addFirst("A");
        t("sub.addFirst.view", m.toString());
        t("sub.addFirst.owner", owner.toString());
        m.addLast("Z");
        t("sub.addLast.view", m.toString());
        t("sub.addLast.owner", owner.toString());
        t("sub.removeFirst", m.removeFirst());
        t("sub.removeLast", m.removeLast());
        t("sub.after.view", m.toString());
        t("sub.after.owner", owner.toString());
        thrown("sub.getFirst.empty", () -> base().subList(2, 2).getFirst());
        thrown("sub.removeLast.empty", () -> base().subList(2, 2).removeLast());

        // --- java/util/Arrays$ArrayList.<init>([Object) ----------------------
        List<String> fixed = Arrays.asList("p", "q", "r");
        t("asList.toString", fixed.toString());
        t("asList.size", fixed.size());
        t("asList.class", fixed.getClass().getName());
        thrown("asList.add", () -> Arrays.asList("p").add("x"));

        // --- java/util/Collections$EmptyListIterator.remove() ----------------
        thrown("emptyLi.remove", () -> Collections.<String>emptyList().listIterator().remove());
        t("emptyLi.hasNext", Collections.<String>emptyList().listIterator().hasNext());
        t("emptyLi.nextIndex", Collections.<String>emptyList().listIterator().nextIndex());

        // --- java/util/Collections$SetFromMap --------------------------------
        Set<String> sfm = Collections.newSetFromMap(new LinkedHashMap<>());
        t("sfm.isEmpty.fresh", sfm.isEmpty());
        t("sfm.size.fresh", sfm.size());
        t("sfm.add", sfm.add("one") + "," + sfm.add("two") + "," + sfm.add("one"));
        t("sfm.size", sfm.size());
        t("sfm.isEmpty", sfm.isEmpty());
        t("sfm.contains.yes", sfm.contains("one"));
        t("sfm.contains.no", sfm.contains("three"));
        List<String> it = new ArrayList<>();
        for (String s : sfm) { it.add(s); }
        Collections.sort(it);
        t("sfm.iterator", it.toString());
        Object[] arr = sfm.toArray();
        String[] as = new String[arr.length];
        for (int i = 0; i < arr.length; i++) { as[i] = String.valueOf(arr[i]); }
        Arrays.sort(as);
        t("sfm.toArray", Arrays.toString(as));
        t("sfm.toArray.len", sfm.toArray().length);
        t("sfm.remove", sfm.remove("one") + "," + sfm.size());

        // --- java/util/LinkedList.isEmpty / stream ---------------------------
        LinkedList<String> ll = new LinkedList<>(List.of("x", "y", "z"));
        t("ll.isEmpty.full", ll.isEmpty());
        t("ll.isEmpty.empty", new LinkedList<String>().isEmpty());
        t("ll.stream", ll.stream().collect(Collectors.joining("+")));
        t("ll.stream.count", ll.stream().count());
        t("ll.stream.empty", new LinkedList<String>().stream().count());

        // --- java/util/Collections static surface, as context ----------------
        t("coll.unmodifiable", Collections.unmodifiableList(base()).toString());
        t("coll.emptyList", Collections.emptyList().toString());
        t("coll.nCopies", Collections.nCopies(3, "n").toString());
        t("coll.frequency", Collections.frequency(base(), "c"));

        System.out.println("rows " + rows);
        System.out.println("DONE L1Wave1Sweep");
    }
}
