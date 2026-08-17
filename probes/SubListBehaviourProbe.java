import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.Comparator;
import java.util.Iterator;
import java.util.LinkedList;
import java.util.List;
import java.util.ListIterator;
import java.util.Vector;
import java.util.stream.Collectors;

/**
 * `List.subList` behaviour, against the host JDK as oracle.
 *
 * The sublist view is the one collection view CratonVM mints under a class
 * `java.base` ALSO instantiates: `ArrayList.subList` runs its own body whenever
 * this VM's native does not intercept it, so the same class carries two
 * populations of object with different layouts. Every method here therefore has
 * to be right for both — a view this VM minted, and one the JDK built — and the
 * failure mode when it is not is a SILENTLY EMPTY list rather than a throw.
 * `probes/JdkOnlyCollectionViewProbe` covers six lines of that; this is the
 * behaviour surface.
 *
 * Each line must print identically under `java`, `cratonvm --real-jdk` and
 * `cratonvm --jdk-only`. Content is always reported, never just success.
 */
public final class SubListBehaviourProbe {

    public static void main(String[] args) {
        reads();
        writeThrough();
        structuralMutation();
        nested();
        comodification();
        iterators();
        otherListTypes();
        jdkBuiltViews();
        System.out.println("SubListBehaviourProbe done");
    }

    static List<String> base() {
        return new ArrayList<>(Arrays.asList("a", "b", "c", "d", "e"));
    }

    static void reads() {
        List<String> b = base();
        List<String> v = b.subList(1, 4);
        p("read.join", () -> join(v));
        p("read.size", () -> "" + v.size());
        p("read.isEmpty", () -> "" + v.isEmpty());
        p("read.get", () -> v.get(0) + "," + v.get(2));
        p("read.get.oob", () -> "" + v.get(3));
        p("read.contains", () -> v.contains("c") + "," + v.contains("a"));
        p("read.indexOf", () -> v.indexOf("c") + "," + v.indexOf("a"));
        p("read.lastIndexOf", () -> "" + v.lastIndexOf("d"));
        p("read.containsAll", () -> "" + v.containsAll(Arrays.asList("b", "d")));
        p("read.toString", () -> v.toString());
        p("read.toArray", () -> Arrays.toString(v.toArray()));
        p("read.toArrayTyped", () -> Arrays.toString(v.toArray(new String[0])));
        p("read.toArrayGen", () -> Arrays.toString(v.toArray(String[]::new)));
        p("read.hashCode", () -> "" + (v.hashCode() == Arrays.asList("b", "c", "d").hashCode()));
        p("read.equals", () -> "" + v.equals(Arrays.asList("b", "c", "d")));
        p("read.equals.other", () -> "" + Arrays.asList("b", "c", "d").equals(v));
        p("read.stream", () -> v.stream().collect(Collectors.joining("|")));
        p("read.forEach", () -> {
            StringBuilder sb = new StringBuilder();
            v.forEach(sb::append);
            return sb.toString();
        });
        p("read.spliterator", () -> "" + v.spliterator().estimateSize());
        p("read.copyCtor", () -> join(new ArrayList<>(v)));
        p("read.addAllFrom", () -> {
            List<String> sink = new ArrayList<>();
            sink.addAll(v);
            return join(sink);
        });
        p("read.empty", () -> join(b.subList(2, 2)));
        p("read.whole", () -> join(b.subList(0, b.size())));
        p("read.class", () -> v.getClass().getName());
        p("read.isList", () -> "" + (v instanceof List));
        p("read.isRandomAccess", () -> "" + (v instanceof java.util.RandomAccess));
        p("read.parallelStream", () -> v.parallelStream().collect(Collectors.joining("|")));
        p("read.getFirst", () -> v.getFirst());
        p("read.getLast", () -> v.getLast());
        p("read.reversed", () -> join(v.reversed()));
        p("read.subListOfWhole", () -> join(b.subList(0, 5).subList(0, 5)));
    }

    static void writeThrough() {
        List<String> b = base();
        List<String> v = b.subList(1, 4);
        p("write.set", () -> {
            v.set(1, "C");
            return join(v) + " base=" + join(b);
        });
        p("write.sort", () -> {
            List<String> bb = new ArrayList<>(Arrays.asList("9", "5", "1", "7", "3"));
            bb.subList(1, 4).sort(Comparator.naturalOrder());
            return join(bb);
        });
        p("write.replaceAll", () -> {
            List<String> bb = base();
            bb.subList(1, 3).replaceAll(String::toUpperCase);
            return join(bb);
        });
        p("write.fill", () -> {
            List<String> bb = base();
            Collections.fill(bb.subList(1, 3), "z");
            return join(bb);
        });
        p("write.reverse", () -> {
            List<String> bb = base();
            Collections.reverse(bb.subList(1, 4));
            return join(bb);
        });
        p("write.swap", () -> {
            List<String> bb = base();
            Collections.swap(bb.subList(1, 4), 0, 2);
            return join(bb);
        });
    }

    static void structuralMutation() {
        p("struct.clear", () -> {
            List<String> b = base();
            b.subList(1, 4).clear();
            return join(b);
        });
        p("struct.add", () -> {
            List<String> b = base();
            List<String> v = b.subList(1, 3);
            v.add("X");
            return join(v) + " base=" + join(b);
        });
        p("struct.addAt", () -> {
            List<String> b = base();
            List<String> v = b.subList(1, 3);
            v.add(0, "X");
            return join(v) + " base=" + join(b);
        });
        p("struct.removeIndex", () -> {
            List<String> b = base();
            List<String> v = b.subList(1, 4);
            String r = v.remove(1);
            return r + " " + join(v) + " base=" + join(b);
        });
        p("struct.removeObject", () -> {
            List<String> b = base();
            List<String> v = b.subList(1, 4);
            boolean r = v.remove("c");
            return r + " " + join(v) + " base=" + join(b);
        });
        p("struct.addAll", () -> {
            List<String> b = base();
            List<String> v = b.subList(1, 3);
            v.addAll(Arrays.asList("X", "Y"));
            return join(v) + " base=" + join(b);
        });
        p("struct.removeIf", () -> {
            List<String> b = base();
            List<String> v = b.subList(1, 4);
            v.removeIf(s -> s.equals("c"));
            return join(v) + " base=" + join(b);
        });
        p("struct.removeAll", () -> {
            List<String> b = base();
            List<String> v = b.subList(1, 4);
            v.removeAll(Arrays.asList("b", "d"));
            return join(v) + " base=" + join(b);
        });
        p("struct.retainAll", () -> {
            List<String> b = base();
            List<String> v = b.subList(1, 4);
            v.retainAll(Arrays.asList("c"));
            return join(v) + " base=" + join(b);
        });
        p("struct.addAllAt", () -> {
            List<String> b = base();
            List<String> v = b.subList(1, 3);
            v.addAll(1, Arrays.asList("X", "Y"));
            return join(v) + " base=" + join(b);
        });
        p("struct.addFirst", () -> {
            List<String> b = base();
            List<String> v = b.subList(1, 3);
            v.addFirst("X");
            return join(v) + " base=" + join(b);
        });
        p("struct.addLast", () -> {
            List<String> b = base();
            List<String> v = b.subList(1, 3);
            v.addLast("X");
            return join(v) + " base=" + join(b);
        });
        p("struct.removeFirst", () -> {
            List<String> b = base();
            List<String> v = b.subList(1, 4);
            String r = v.removeFirst();
            return r + " " + join(v) + " base=" + join(b);
        });
        p("struct.removeLast", () -> {
            List<String> b = base();
            List<String> v = b.subList(1, 4);
            String r = v.removeLast();
            return r + " " + join(v) + " base=" + join(b);
        });
    }

    static void nested() {
        p("nested.read", () -> {
            List<String> b = base();
            return join(b.subList(1, 5).subList(1, 3));
        });
        p("nested.class", () -> base().subList(1, 5).subList(1, 3).getClass().getName());
        p("nested.set", () -> {
            List<String> b = base();
            b.subList(1, 5).subList(1, 3).set(0, "C");
            return join(b);
        });
        p("nested.clear", () -> {
            List<String> b = base();
            List<String> outer = b.subList(1, 5);
            outer.subList(1, 3).clear();
            return join(outer) + " base=" + join(b);
        });
        p("nested.triple", () -> {
            List<String> b = base();
            return join(b.subList(0, 5).subList(1, 4).subList(1, 3));
        });
    }

    static void comodification() {
        p("comod.afterParentAdd", () -> {
            List<String> b = base();
            List<String> v = b.subList(1, 3);
            b.add("f");
            return join(v);
        });
        p("comod.afterParentRemove", () -> {
            List<String> b = base();
            List<String> v = b.subList(1, 3);
            b.remove(0);
            return "" + v.size();
        });
        p("comod.afterParentSet", () -> {
            List<String> b = base();
            List<String> v = b.subList(1, 3);
            b.set(1, "B");
            return join(v);
        });
        p("comod.viewSurvivesOwnMutation", () -> {
            List<String> b = base();
            List<String> v = b.subList(1, 4);
            v.remove(0);
            return join(v) + " base=" + join(b);
        });
    }

    static void iterators() {
        List<String> b = base();
        List<String> v = b.subList(1, 4);
        p("iter.forEachLoop", () -> {
            StringBuilder sb = new StringBuilder();
            for (String s : v) {
                sb.append(s);
            }
            return sb.toString();
        });
        p("iter.explicit", () -> {
            Iterator<String> it = v.iterator();
            StringBuilder sb = new StringBuilder();
            while (it.hasNext()) {
                sb.append(it.next());
            }
            return sb.toString();
        });
        p("iter.listIterator", () -> {
            ListIterator<String> it = v.listIterator();
            StringBuilder sb = new StringBuilder();
            while (it.hasNext()) {
                sb.append(it.nextIndex()).append(it.next());
            }
            return sb.toString();
        });
        p("iter.listIteratorFrom", () -> {
            ListIterator<String> it = v.listIterator(1);
            return it.next() + "," + it.previous();
        });
        p("iter.remove", () -> {
            List<String> bb = base();
            List<String> vv = bb.subList(1, 4);
            Iterator<String> it = vv.iterator();
            it.next();
            it.remove();
            return join(vv) + " base=" + join(bb);
        });
        p("iter.removeSecond", () -> {
            List<String> bb = base();
            List<String> vv = bb.subList(1, 4);
            Iterator<String> it = vv.iterator();
            it.next();
            it.next();
            it.remove();
            return join(vv) + " base=" + join(bb);
        });
        p("iter.removeAllViaIterator", () -> {
            List<String> bb = base();
            List<String> vv = bb.subList(1, 4);
            Iterator<String> it = vv.iterator();
            while (it.hasNext()) {
                it.next();
                it.remove();
            }
            return join(vv) + " base=" + join(bb);
        });
        p("iter.removeBeforeNext", () -> {
            List<String> bb = base();
            Iterator<String> it = bb.subList(1, 4).iterator();
            it.remove();
            return "no-throw";
        });
        p("iter.exhausted", () -> {
            List<String> bb = base();
            Iterator<String> it = bb.subList(1, 2).iterator();
            it.next();
            return "" + it.hasNext() + "," + nextOrThrow(it);
        });
        p("iter.listIterator.set", () -> {
            List<String> bb = base();
            List<String> vv = bb.subList(1, 4);
            ListIterator<String> it = vv.listIterator();
            it.next();
            it.set("B");
            return join(vv) + " base=" + join(bb);
        });
        p("iter.listIterator.remove", () -> {
            List<String> bb = base();
            List<String> vv = bb.subList(1, 4);
            ListIterator<String> it = vv.listIterator();
            it.next();
            it.remove();
            return join(vv) + " base=" + join(bb);
        });
        p("iter.listIterator.add", () -> {
            List<String> bb = base();
            List<String> vv = bb.subList(1, 4);
            ListIterator<String> it = vv.listIterator();
            it.next();
            it.add("X");
            return join(vv) + " base=" + join(bb);
        });
        p("iter.listIterator.backwards", () -> {
            List<String> bb = base();
            ListIterator<String> it = bb.subList(1, 4).listIterator(3);
            StringBuilder sb = new StringBuilder();
            while (it.hasPrevious()) {
                sb.append(it.previousIndex()).append(it.previous());
            }
            return sb.toString();
        });
    }

    static String nextOrThrow(Iterator<String> it) {
        try {
            return it.next();
        } catch (Throwable e) {
            return e.getClass().getName();
        }
    }

    static void otherListTypes() {
        p("other.linkedList", () -> {
            List<String> l = new LinkedList<>(Arrays.asList("a", "b", "c", "d"));
            return join(l.subList(1, 3)) + " " + l.subList(1, 3).getClass().getName();
        });
        p("other.vector", () -> {
            List<String> l = new Vector<>(Arrays.asList("a", "b", "c", "d"));
            return join(l.subList(1, 3));
        });
        p("other.asList", () -> join(Arrays.asList("a", "b", "c", "d").subList(1, 3)));
        p("other.unmodifiable", () -> {
            List<String> l = Collections.unmodifiableList(new ArrayList<>(Arrays.asList("a", "b", "c")));
            return join(l.subList(0, 2));
        });
        p("other.listOf", () -> join(List.of("a", "b", "c").subList(0, 2)));
        p("other.singleton", () -> join(Collections.singletonList("s").subList(0, 1)));
        p("other.emptyList", () -> join(Collections.<String>emptyList().subList(0, 0)));
    }

    /**
     * Paths where java.base's own code builds the sublist, so this VM's natives
     * meet a receiver they did not mint. These are the lines a carrier change
     * without a receiver test turns into `null`.
     */
    static void jdkBuiltViews() {
        p("jdk.patternSplit", () -> String.join("|", java.util.regex.Pattern.compile(",").split("1,2,3")));
        p("jdk.stringSplit", () -> String.join("|", "6,7".split(",")));
        p("jdk.splitLimit", () -> String.join("|", "a:b:c".split(":", 2)));
        p("jdk.splitTrailing", () -> "" + "a,b,,".split(",").length);
        p("jdk.patternSplitAsStream",
            () -> java.util.regex.Pattern.compile(",").splitAsStream("4,5").collect(Collectors.joining("|")));
        p("jdk.listSort", () -> {
            List<String> l = new ArrayList<>(Arrays.asList("c", "a", "b"));
            Collections.sort(l);
            return join(l);
        });
        p("jdk.binarySearch", () -> {
            List<String> l = new ArrayList<>(Arrays.asList("a", "b", "c", "d"));
            return "" + Collections.binarySearch(l.subList(1, 4), "c");
        });
        p("jdk.indexOfSubList", () -> {
            List<String> l = new ArrayList<>(Arrays.asList("a", "b", "c", "d"));
            return "" + Collections.indexOfSubList(l, l.subList(1, 3));
        });
        p("jdk.streamToList", () -> join(base().stream().skip(1).limit(3).collect(Collectors.toList())));
    }

    interface T {
        String get() throws Throwable;
    }

    static void p(String label, T t) {
        try {
            System.out.println(label + "=" + t.get());
        } catch (Throwable e) {
            String msg = e.getMessage();
            System.out.println(label + "=" + e.getClass().getName() + (msg == null ? "" : ": " + msg));
        }
    }

    static String join(java.util.Collection<?> c) {
        StringBuilder sb = new StringBuilder();
        for (Object o : c) {
            if (sb.length() > 0) {
                sb.append('|');
            }
            sb.append(o);
        }
        return "[" + sb + "]/" + c.size();
    }
}
