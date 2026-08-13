import java.util.ArrayList;
import java.util.HashMap;
import java.util.Collection;
import java.util.List;

/**
 * Per-call cost of the `native_al_*` shim family against the identical method
 * body written in plain Java on the same VM.
 *
 * Every loop lives in its OWN static method (a loop in `main` measures
 * interpreted code only). Rows are ns/op per block so tier-up is visible.
 */
public final class AlCallCostProbe {

    /** The same two fields ArrayList has, with the same bodies, in plain Java. */
    static final class MyList {
        Object[] elementData;
        int size;
        MyList(int cap) { elementData = new Object[cap]; }
        int size() { return size; }
        Object get(int i) { return elementData[i]; }
        boolean isEmpty() { return size == 0; }
        void add(Object o) { elementData[size++] = o; }
    }

    private static ArrayList<Object> AL;
    private static MyList ML;
    private static Collection<Object> VALUES;
    private static List<Object> SUBLIST;
    private static long sink;

    private static void alSize(int n) { long s = 0; for (int i = 0; i < n; i++) { s += AL.size(); } sink += s; }
    private static void mlSize(int n) { long s = 0; for (int i = 0; i < n; i++) { s += ML.size(); } sink += s; }
    private static void alGet(int n)  { long s = 0; for (int i = 0; i < n; i++) { s += AL.get(i & 63) == null ? 0 : 1; } sink += s; }
    private static void mlGet(int n)  { long s = 0; for (int i = 0; i < n; i++) { s += ML.get(i & 63) == null ? 0 : 1; } sink += s; }
    private static void alIsEmpty(int n) { long s = 0; for (int i = 0; i < n; i++) { s += AL.isEmpty() ? 1 : 0; } sink += s; }
    private static void mlIsEmpty(int n) { long s = 0; for (int i = 0; i < n; i++) { s += ML.isEmpty() ? 1 : 0; } sink += s; }
    private static void valuesSize(int n) { long s = 0; for (int i = 0; i < n; i++) { s += VALUES.size(); } sink += s; }
    private static void subListSize(int n) { long s = 0; for (int i = 0; i < n; i++) { s += SUBLIST.size(); } sink += s; }
    private static void alAddRemove(int n) {
        long s = 0;
        ArrayList<Object> t = new ArrayList<>();
        for (int i = 0; i < n; i++) { t.add(this0); if (t.size() > 32) { t.clear(); } s += t.size(); }
        sink += s;
    }
    private static final Object this0 = new Object();

    public static void main(String[] args) {
        int blocks = args.length > 0 ? Integer.parseInt(args[0]) : 4;
        int bs = args.length > 1 ? Integer.parseInt(args[1]) : 200_000;

        AL = new ArrayList<>();
        ML = new MyList(128);
        for (int i = 0; i < 64; i++) { AL.add("e" + i); ML.add("e" + i); }
        HashMap<String, Object> hm = new HashMap<>();
        for (int i = 0; i < 64; i++) { hm.put("k" + i, "v" + i); }
        VALUES = hm.values();
        SUBLIST = AL.subList(0, 32);

        String[] names = {"MyList.size", "ArrayList.size", "MyList.get", "ArrayList.get",
                          "MyList.isEmpty", "ArrayList.isEmpty", "values().size", "subList().size",
                          "ArrayList.add+size"};
        System.out.printf("%-22s", "ns/op by block");
        for (int b = 0; b < blocks; b++) { System.out.printf("%9d", b); }
        System.out.println();
        for (int k = 0; k < names.length; k++) {
            StringBuilder out = new StringBuilder(String.format("%-22s", names[k]));
            for (int b = 0; b < blocks; b++) {
                long t0 = System.nanoTime();
                switch (k) {
                    case 0: mlSize(bs); break;
                    case 1: alSize(bs); break;
                    case 2: mlGet(bs); break;
                    case 3: alGet(bs); break;
                    case 4: mlIsEmpty(bs); break;
                    case 5: alIsEmpty(bs); break;
                    case 6: valuesSize(bs); break;
                    case 7: subListSize(bs); break;
                    default: alAddRemove(bs); break;
                }
                out.append(String.format("%9d", (System.nanoTime() - t0) / bs));
            }
            System.out.println(out);
        }
        System.out.println("sink=" + sink);
    }
}
