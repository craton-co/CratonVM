import java.util.ArrayDeque;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Queue;

/**
 * netty AhoCorasicSearchProcessorFactory's two build halves, copied verbatim
 * and timed separately: buildTrie (ArrayList&lt;Integer&gt;-bound) and
 * linkSuffixes (int[]-bound). Answers which half owns CratonVM's ~700x gap on
 * SearchProcessorTest.testUniqueLen64Substrings.
 */
public final class TrieSplit {

    static final int ALPHABET_SIZE = 256;
    static final int BITS_PER_SYMBOL = 8;

    static int[] jumpTable;
    static int[] matchForNeedleId;

    static void buildTrie(byte[] needle) {
        ArrayList<Integer> jumpTableBuilder = new ArrayList<Integer>(ALPHABET_SIZE);
        for (int i = 0; i < ALPHABET_SIZE; i++) {
            jumpTableBuilder.add(-1);
        }
        ArrayList<Integer> matchForBuilder = new ArrayList<Integer>();
        matchForBuilder.add(-1);

        int currentPosition = 0;
        for (byte ch0 : needle) {
            final int ch = ch0 & 0xff;
            final int next = currentPosition + ch;
            if (jumpTableBuilder.get(next) == -1) {
                jumpTableBuilder.set(next, jumpTableBuilder.size());
                for (int i = 0; i < ALPHABET_SIZE; i++) {
                    jumpTableBuilder.add(-1);
                }
                matchForBuilder.add(-1);
            }
            currentPosition = jumpTableBuilder.get(next);
        }
        matchForBuilder.set(currentPosition >> BITS_PER_SYMBOL, 0);

        jumpTable = new int[jumpTableBuilder.size()];
        for (int i = 0; i < jumpTableBuilder.size(); i++) {
            jumpTable[i] = jumpTableBuilder.get(i);
        }
        matchForNeedleId = new int[matchForBuilder.size()];
        for (int i = 0; i < matchForBuilder.size(); i++) {
            matchForNeedleId[i] = matchForBuilder.get(i);
        }
    }

    static void linkSuffixes() {
        Queue<Integer> queue = new ArrayDeque<Integer>();
        queue.add(0);
        int[] suffixLinks = new int[matchForNeedleId.length];
        Arrays.fill(suffixLinks, -1);
        while (!queue.isEmpty()) {
            final int v = queue.remove();
            int vPosition = v >> BITS_PER_SYMBOL;
            final int u = suffixLinks[vPosition] == -1 ? 0 : suffixLinks[vPosition];
            if (matchForNeedleId[vPosition] == -1) {
                matchForNeedleId[vPosition] = matchForNeedleId[u >> BITS_PER_SYMBOL];
            }
            for (int ch = 0; ch < ALPHABET_SIZE; ch++) {
                final int vIndex = v | ch;
                final int uIndex = u | ch;
                final int jumpV = jumpTable[vIndex];
                final int jumpU = jumpTable[uIndex];
                if (jumpV != -1) {
                    suffixLinks[jumpV >> BITS_PER_SYMBOL] = v > 0 && jumpU != -1 ? jumpU : 0;
                    queue.add(jumpV);
                } else {
                    jumpTable[vIndex] = jumpU != -1 ? jumpU : 0;
                }
            }
        }
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
        long build = 0;
        long link = 0;
        long guard = 0;
        for (int s = 0; s < n; s++) {
            byte[] needle = new byte[64];
            System.arraycopy(haystack, s, needle, 0, 64);
            long t0 = System.nanoTime();
            buildTrie(needle);
            long t1 = System.nanoTime();
            linkSuffixes();
            long t2 = System.nanoTime();
            build += t1 - t0;
            link += t2 - t1;
            guard += jumpTable.length;
        }
        System.out.println("@@ n=" + n + " buildTrie_ms=" + (build / 1000000L)
                + " linkSuffixes_ms=" + (link / 1000000L) + " guard=" + guard);
    }
}
