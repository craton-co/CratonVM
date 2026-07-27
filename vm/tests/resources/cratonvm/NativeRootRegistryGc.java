package cratonvm;

import java.util.LinkedHashMap;
import java.util.LinkedList;
import java.util.Locale;
import java.util.TreeMap;
import java.util.TreeSet;

/** Exercises native side-table roots across repeated young and full GCs. */
public final class NativeRootRegistryGc {
    public static void main(String[] args) {
        LinkedList<String> list = new LinkedList<>();
        LinkedHashMap<Integer, String> insertionOrder = new LinkedHashMap<>();
        TreeMap<Integer, String> sorted = new TreeMap<>();
        TreeSet<Integer> set = new TreeSet<>();
        long checksum = 0;

        for (int round = 0; round < 240; round++) {
            String value = "root-" + round + "-" + (round * 104729L);
            list.add(value);
            insertionOrder.put(round, value);
            sorted.put(round, value);
            if (!set.add(round)) {
                throw new AssertionError("TreeSet rejected fresh value at round " + round);
            }

            if (round >= 32) {
                int expired = round - 32;
                String expected = "root-" + expired + "-" + (expired * 104729L);
                if (!expected.equals(list.removeFirst())
                        || !expected.equals(insertionOrder.remove(expired))
                        || !expected.equals(sorted.remove(expired))
                        || !set.remove(expired)) {
                    throw new AssertionError("overlay content changed at round " + round);
                }
            }

            Object[] churn = new Object[32];
            for (int i = 0; i < churn.length; i++) {
                churn[i] = new byte[1024 + ((round + i) & 255)];
            }
            if ((round & 3) == 0) {
                System.gc();
            }

            int newest = round;
            if (!value.equals(insertionOrder.get(newest))) {
                throw new AssertionError("LinkedHashMap root lost at round " + round);
            }
            if (!value.equals(sorted.get(newest))) {
                throw new AssertionError("TreeMap root lost at round " + round);
            }
            if (!set.contains(newest)) {
                throw new AssertionError("TreeSet root lost at round " + round);
            }
            if (list.size() != insertionOrder.size()
                    || list.size() != sorted.size()
                    || list.size() != set.size()) {
                throw new AssertionError(
                        "size mismatch at round "
                                + round
                                + ": list="
                                + list.size()
                                + ", linked="
                                + insertionOrder.size()
                                + ", tree="
                                + sorted.size()
                                + ", set="
                                + set.size());
            }
            checksum = checksum * 31 + value.hashCode();
            checksum ^= Locale.getDefault().toString().hashCode();
            checksum ^= System.getProperties().size();
            checksum ^= System.getenv().size();
        }

        System.out.println(
                "NATIVE_ROOT_REGISTRY_GC_OK size=" + list.size() + " checksum=" + checksum);
    }
}
