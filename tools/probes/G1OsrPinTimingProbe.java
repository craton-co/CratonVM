import java.lang.ref.WeakReference;
import java.util.ArrayList;
import java.util.List;

public final class G1OsrPinTimingProbe {
    static List<long[]> data;
    static long[] first;
    static long sink;

    static WeakReference<long[]> stage() {
        first = new long[128];
        return new WeakReference<>(first);
    }

    static boolean fillAndCollectInside(WeakReference<?> ref) {
        data = new ArrayList<>();
        data.add(first);
        first = null;
        try {
            while (true) data.add(new long[128]);
        } catch (OutOfMemoryError expected) {
            for (int i = 0; i < 64 && data.size() > 1; i++) data.remove(data.size() - 1);
        }
        data = null;
        System.gc();
        System.gc();
        return ref.get() == null;
    }

    static void fillAndReturn() {
        data = new ArrayList<>();
        data.add(first);
        first = null;
        try {
            while (true) data.add(new long[128]);
        } catch (OutOfMemoryError expected) {
            for (int i = 0; i < 64 && data.size() > 1; i++) data.remove(data.size() - 1);
        }
        data = null;
    }

    static boolean afterReturn(WeakReference<?> ref) {
        System.gc();
        System.gc();
        return ref.get() == null;
    }

    public static void main(String[] args) {
        WeakReference<long[]> inside = stage();
        System.out.println("inside-clear=" + fillAndCollectInside(inside));
        inside.clear();
        WeakReference<long[]> after = stage();
        fillAndReturn();
        System.out.println("after-return-clear=" + afterReturn(after));
    }
}
