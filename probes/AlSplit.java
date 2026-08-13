import java.util.ArrayList;

/**
 * netty AhoCorasicSearchProcessorFactory.buildTrie's exact ArrayList shape,
 * run against java.util.ArrayList and against a byte-for-byte equivalent
 * hand-rolled list. The ratio is the share of the trie build that CratonVM's
 * ArrayList natives own; on HotSpot the two are within noise of each other.
 */
public final class AlSplit {

    static final int ALPHABET_SIZE = 256;
    static final int BITS_PER_SYMBOL = 8;

    /** Same body as java.util.ArrayList's, with no native registration behind it. */
    static final class MyList {
        Object[] elementData = new Object[10];
        int size;

        int size() {
            return size;
        }

        Object get(int index) {
            return elementData[index];
        }

        boolean add(Object e) {
            if (size == elementData.length) {
                Object[] grown = new Object[size + (size >> 1) + 1];
                System.arraycopy(elementData, 0, grown, 0, size);
                elementData = grown;
            }
            elementData[size++] = e;
            return true;
        }

        Object set(int index, Object e) {
            Object old = elementData[index];
            elementData[index] = e;
            return old;
        }
    }

    static long realList(byte[] needle) {
        ArrayList<Integer> jump = new ArrayList<Integer>(ALPHABET_SIZE);
        for (int i = 0; i < ALPHABET_SIZE; i++) {
            jump.add(-1);
        }
        int currentPosition = 0;
        for (byte ch0 : needle) {
            final int ch = ch0 & 0xff;
            final int next = currentPosition + ch;
            if (jump.get(next) == -1) {
                jump.set(next, jump.size());
                for (int i = 0; i < ALPHABET_SIZE; i++) {
                    jump.add(-1);
                }
            }
            currentPosition = jump.get(next);
        }
        long sum = 0;
        for (int i = 0; i < jump.size(); i++) {
            sum += jump.get(i);
        }
        return sum;
    }

    static long myList(byte[] needle) {
        MyList jump = new MyList();
        for (int i = 0; i < ALPHABET_SIZE; i++) {
            jump.add(-1);
        }
        int currentPosition = 0;
        for (byte ch0 : needle) {
            final int ch = ch0 & 0xff;
            final int next = currentPosition + ch;
            if (((Integer) jump.get(next)).intValue() == -1) {
                jump.set(next, Integer.valueOf(jump.size()));
                for (int i = 0; i < ALPHABET_SIZE; i++) {
                    jump.add(-1);
                }
            }
            currentPosition = ((Integer) jump.get(next)).intValue();
        }
        long sum = 0;
        for (int i = 0; i < jump.size(); i++) {
            sum += ((Integer) jump.get(i)).intValue();
        }
        return sum;
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 60;
        byte[] haystack = new byte[32 * 65];
        int pos = 0;
        for (int i = 1; i <= 64; i++) {
            for (int j = 0; j < i; j++) {
                haystack[pos++] = (byte) i;
            }
        }
        long guard = 0;
        long t0 = System.nanoTime();
        for (int s = 0; s < n; s++) {
            byte[] needle = new byte[64];
            System.arraycopy(haystack, s, needle, 0, 64);
            guard += realList(needle);
        }
        long t1 = System.nanoTime();
        for (int s = 0; s < n; s++) {
            byte[] needle = new byte[64];
            System.arraycopy(haystack, s, needle, 0, 64);
            guard += myList(needle);
        }
        long t2 = System.nanoTime();
        System.out.println("@@ n=" + n
                + " arraylist_ms=" + ((t1 - t0) / 1000000L)
                + " handrolled_ms=" + ((t2 - t1) / 1000000L)
                + " guard=" + guard);
    }
}
