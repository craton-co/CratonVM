public class Tern3 {
    static int loopAdd(int n) {
        int i = 0;
        int visits = 0;
        while (i < n - 1) {
            if (i == 1) return -1;            // we expect to skip i==1
            visits++;
            i += i == 0 ? 2 : 1;              // first step is +2, then +1 forever
        }
        return visits;
    }
    public static void main(String[] args) {
        int iters = Integer.parseInt(args[0]);
        for (int t = 0; t < iters; t++) {
            int v = loopAdd(256);
            if (v < 0) { System.out.println("BAD at trial " + t); return; }
        }
        System.out.println("OK " + iters);
    }
}
