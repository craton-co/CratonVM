import java.util.ArrayList;
import java.util.List;

public class LoopDupOsrNoIndy {
    public static void main(String[] args) throws Exception {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 4000;
        List<Integer> keys = new ArrayList<>(n);
        for (int i = 0; i < n; i++) {
            keys.add(i);
        }
        // No string concatenation (no invokedynamic) here on purpose --
        // print each part with a separate call instead of "+".
        System.out.print("n=");
        System.out.print(n);
        System.out.print(" keys.size()=");
        System.out.println(keys.size());
        long sum = 0;
        for (Integer k : keys) {
            sum += k;
        }
        System.out.print("sum=");
        System.out.println(sum);
    }
}
