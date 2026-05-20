package cratonvm;

/**
 * Minimal threading test — verifies Thread.start()/join() + static field write.
 */
public class ThreadBasicTest implements Runnable {
    static int result = 0;

    public void run() {
        result = 42;
    }

    public static int testThreadBasic() {
        result = 0;
        Thread t = new Thread(new ThreadBasicTest());
        t.start();
        try { t.join(); } catch (Exception e) { return -1; }
        return result;
    }
}
