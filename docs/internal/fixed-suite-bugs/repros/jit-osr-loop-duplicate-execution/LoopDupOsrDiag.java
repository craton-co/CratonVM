import java.util.ArrayList;
import java.util.List;

public class LoopDupOsrDiag {
    public static void main(String[] args) throws Exception {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 4000;
        List<Integer> keys = new ArrayList<>(n);
        for (int i = 0; i < n; i++) {
            keys.add(i);
        }
        System.out.println("n=" + n + " keys.size()=" + keys.size());
        // Find first index where the recorded value stops matching its
        // position, and report the run of values seen after that point so we
        // can tell whether ranges are repeated, skipped, or jumbled.
        int firstMismatch = -1;
        for (int idx = 0; idx < keys.size(); idx++) {
            if (keys.get(idx) != idx) {
                firstMismatch = idx;
                break;
            }
        }
        System.out.println("firstMismatch=" + firstMismatch);
        if (firstMismatch >= 0) {
            int lo = Math.max(0, firstMismatch - 5);
            int hi = Math.min(keys.size(), firstMismatch + 30);
            StringBuilder sb = new StringBuilder();
            for (int idx = lo; idx < hi; idx++) {
                sb.append(idx).append(':').append(keys.get(idx)).append(' ');
            }
            System.out.println("around: " + sb);
        }
        long sum = 0;
        for (Integer k : keys) {
            sum += k;
        }
        System.out.println("sum=" + sum);
    }
}
