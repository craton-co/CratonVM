public class Spawn {
    public static void main(String[] a) throws Exception {
        for (int round = 0; round < 50; round++) {
            Thread[] ts = new Thread[8];
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
        System.out.println("DONE");
    }
}
