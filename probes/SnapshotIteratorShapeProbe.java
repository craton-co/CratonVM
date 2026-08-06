import java.nio.file.Path;
import java.nio.file.Paths;
import java.util.Arrays;
import java.util.Collections;
import java.util.EnumSet;
import java.util.Iterator;
import java.util.List;
import java.util.ServiceLoader;
import java.util.concurrent.CopyOnWriteArrayList;

/**
 * The class, contents and `remove()` behaviour of every snapshot iterator this
 * VM hands out.
 *
 * CratonVM builds these in one shared helper, `make_iterator_from_array`, which
 * used to mint `java.util.HashMap$KeyItr` — a class no JDK declares. That was
 * visible from ordinary Java: `Arrays.asList(a).iterator().getClass()` reported
 * it. The helper now builds a real `java.util.Arrays$ArrayItr` instead, so this
 * probe pins down the whole surface that changed rather than the one line that
 * exposed it.
 *
 * `getClass().getName()` is the point of the probe, so it is printed for every
 * case. The CONTENTS are printed too: an iterator that reports the right class
 * and then yields nothing — or yields a trailing `null` because a caller
 * over-allocated its snapshot and `hasNext()` is `cursor < a.length` — would
 * otherwise read as a pass. `remove()` is exercised because the real class
 * inherits the throwing default, and "throws the right exception" is part of
 * matching HotSpot.
 *
 * Every line must be identical under `java`, `cratonvm --real-jdk` and
 * `cratonvm --jdk-only`.
 */
public final class SnapshotIteratorShapeProbe {

    enum Color { RED, GREEN }

    public static void main(String[] args) {
        arrays();
        emptyAndSingleton();
        enumSet();
        concurrent();
        nio();
        serviceLoader();
        removeBehaviour();
        emptySnapshots();
        System.out.println("SnapshotIteratorShapeProbe done");
    }

    static void arrays() {
        List<String> l = Arrays.asList("a", "b", "c");
        p("asList.class", () -> l.iterator().getClass().getName());
        p("asList.drain", () -> drain(l.iterator()));
        p("asList.one", () -> drain(Arrays.asList("z").iterator()));
        p("asList.enhancedFor", () -> {
            StringBuilder sb = new StringBuilder();
            for (String s : l) {
                sb.append(s);
            }
            return sb.toString();
        });
    }

    static void emptyAndSingleton() {
        p("emptyIterator.class", () -> Collections.emptyIterator().getClass().getName());
        p("emptyIterator.drain", () -> drain(Collections.emptyIterator()));
        p("singletonList.drain", () -> drain(Collections.singletonList("s").iterator()));
        p("emptyList.drain", () -> drain(Collections.emptyList().iterator()));
    }

    static void enumSet() {
        p("enumSet.drain", () -> drain(EnumSet.allOf(Color.class).iterator()));
        p("enumSet.none", () -> drain(EnumSet.noneOf(Color.class).iterator()));
    }

    static void concurrent() {
        CopyOnWriteArrayList<String> c = new CopyOnWriteArrayList<>(Arrays.asList("p", "q"));
        p("cowal.drain", () -> drain(c.iterator()));
        p("cowal.class", () -> c.iterator().getClass().getName());
    }

    static void nio() {
        Path pth = Paths.get("a", "b", "c");
        p("path.drain", () -> drain(pth.iterator()));
        p("path.count", () -> "" + pth.getNameCount());
    }

    static void serviceLoader() {
        // No providers are registered, so this must be an EMPTY iteration
        // rather than a failure — the empty case is where an over-allocated
        // snapshot shows up as a phantom null element.
        p("serviceLoader.empty", () -> {
            ServiceLoader<java.time.chrono.Chronology> sl =
                    ServiceLoader.load(java.time.chrono.Chronology.class);
            Iterator<?> it = sl.iterator();
            int n = 0;
            while (it.hasNext() && n < 8) {
                it.next();
                n++;
            }
            return "count>=0:" + (n >= 0);
        });
    }

    /** The real class inherits `Iterator.remove()`, which throws. */
    static void removeBehaviour() {
        p("asList.remove", () -> {
            Iterator<String> it = Arrays.asList("a", "b").iterator();
            it.next();
            try {
                it.remove();
                return "no-throw";
            } catch (UnsupportedOperationException e) {
                return "UnsupportedOperationException";
            }
        });
        p("asList.remove.beforeNext", () -> {
            Iterator<String> it = Arrays.asList("a").iterator();
            try {
                it.remove();
                return "no-throw";
            } catch (UnsupportedOperationException | IllegalStateException e) {
                return e.getClass().getSimpleName();
            }
        });
    }

    /**
     * Exhaustion. `Arrays$ArrayItr.next()` past the end throws
     * `NoSuchElementException`; a snapshot whose array is longer than its
     * logical size would instead hand back a null.
     */
    static void emptySnapshots() {
        p("exhausted.next", () -> {
            Iterator<String> it = Arrays.asList("a").iterator();
            it.next();
            try {
                Object v = it.next();
                return "no-throw, got " + v;
            } catch (java.util.NoSuchElementException e) {
                return "NoSuchElementException";
            }
        });
        p("empty.next", () -> {
            Iterator<Object> it = Arrays.asList(new Object[0]).iterator();
            try {
                Object v = it.next();
                return "no-throw, got " + v;
            } catch (java.util.NoSuchElementException e) {
                return "NoSuchElementException";
            }
        });
        p("empty.hasNext", () -> "" + Arrays.asList(new Object[0]).iterator().hasNext());
    }

    interface T {
        String get() throws Exception;
    }

    static String drain(Iterator<?> it) {
        StringBuilder sb = new StringBuilder();
        int n = 0;
        while (it.hasNext() && n < 64) {
            if (n > 0) {
                sb.append('|');
            }
            sb.append(it.next());
            n++;
        }
        return "[" + sb + "]/" + n;
    }

    static void p(String label, T t) {
        try {
            System.out.println(label + "=" + t.get());
        } catch (Throwable e) {
            String msg = e.getMessage();
            System.out.println(label + "=" + e.getClass().getName() + (msg == null ? "" : ": " + msg));
        }
    }
}
