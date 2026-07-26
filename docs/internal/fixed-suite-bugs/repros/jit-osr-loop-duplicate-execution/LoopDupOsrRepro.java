import java.util.ArrayList;
import java.util.List;

public class LoopDupOsrRepro {
    public static void main(String[] args) throws Exception {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 4000;
        List<Object> keys = new ArrayList<>(n);
        for (int i = 0; i < n; i++) {
            keys.add(new Object());
        }
        System.out.println("n=" + n + " keys.size()=" + keys.size());
        long sum = 0;
        for (Object k : keys) {
            sum += k.hashCode() == 0 ? 1 : 0;
        }
        System.out.println("sum=" + sum);
    }
}
