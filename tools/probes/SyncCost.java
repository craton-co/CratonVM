/** Cost of an UNCONTENDED synchronized method / block against the same work unsynchronized. */
public class SyncCost {
    private int v = 7;
    synchronized int syncGet() { return v; }
    int plainGet() { return v; }
    static int sink;

    public static void main(String[] args) throws Exception {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 2000000;
        SyncCost o = new SyncCost();
        StringBuffer sb = new StringBuffer("SELECT CUSTOM_PR");
        StringBuilder sbu = new StringBuilder("SELECT CUSTOM_PR");
        for (int r = 0; r < 3; r++) {
            long a = System.nanoTime();
            for (int i = 0; i < n; i++) { sink += o.plainGet(); }
            long b = System.nanoTime();
            for (int i = 0; i < n; i++) { sink += o.syncGet(); }
            long c = System.nanoTime();
            for (int i = 0; i < n; i++) { synchronized (o) { sink += o.v; } }
            long d = System.nanoTime();
            for (int i = 0; i < n; i++) { sink += sbu.charAt(3); }
            long e = System.nanoTime();
            for (int i = 0; i < n; i++) { sink += sb.charAt(3); }
            long f = System.nanoTime();
            System.out.println("n=" + n
                    + " plain=" + ((b - a) / 1000000) + "ms"
                    + " syncMethod=" + ((c - b) / 1000000) + "ms"
                    + " syncBlock=" + ((d - c) / 1000000) + "ms"
                    + " SBuilder.charAt=" + ((e - d) / 1000000) + "ms"
                    + " SBuffer.charAt=" + ((f - e) / 1000000) + "ms");
        }
    }
}
