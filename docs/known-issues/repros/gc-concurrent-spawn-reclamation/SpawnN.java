// Configurable concurrent-spawn repro: SpawnN <threads> <rounds>
public class SpawnN {
    public static void main(String[] a) throws Exception {
        int n = a.length > 0 ? Integer.parseInt(a[0]) : 8;
        int rounds = a.length > 1 ? Integer.parseInt(a[1]) : 50;
        for (int round = 0; round < rounds; round++) {
            Thread[] ts = new Thread[n];
            for (int i = 0; i < ts.length; i++) {
                final int id = i;
                ts[i] = new Thread(() -> {
                    long s = 0;
                    for (int k = 0; k < 5000; k++) s += (k ^ id);
                    if (s == -1) System.out.println("x");
                });
                ts[i].start();
            }
            for (Thread t : ts) t.join();
        }
        System.out.println("DONE n=" + n + " rounds=" + rounds);
    }
}
