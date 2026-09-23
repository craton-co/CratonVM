// h23 verification probe for
// docs/known-issues/jit/aarch64-backend-runs-no-java-on-a-real-machine-20260922.md
// #1: a hot loop whose body is guarded by a try/catch (a non-empty exception
// table), and which actually throws and catches on a real path -- not just a
// dead one -- so a wrong local-handler routing corrupts the printed total
// instead of merely failing to compile.
public class A64ExcTableProbe {
    static int work(int n) {
        int total = 0;
        for (int i = 0; i < n; i++) {
            try {
                if (i % 7 == 0) {
                    throw new RuntimeException("seventh");
                }
                total += i;
            } catch (RuntimeException e) {
                total -= 1;
            }
        }
        return total;
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 2000;
        int iters = args.length > 1 ? Integer.parseInt(args[1]) : 200;
        long sum = 0;
        for (int i = 0; i < iters; i++) {
            sum += work(n);
        }
        System.out.println(sum);
    }
}
