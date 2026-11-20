// gc-common w12-e fixture: `main` starts a non-daemon thread and then throws.
// HotSpot prints the uncaught trace, waits for the thread (DestroyJavaVM), so
// KEEPER-DONE is printed, and exits with 1. Compiled with `javac --release 21`.
// A subclass of Thread (no lambda) keeps it runnable without invokedynamic.
public class UncaughtKeeper extends Thread {
    @Override
    public void run() {
        try {
            Thread.sleep(300);
        } catch (InterruptedException e) {
            // fall through: the line below is what the test looks for
        }
        System.out.println("KEEPER-DONE");
    }

    public static void main(String[] args) {
        new UncaughtKeeper().start();
        throw new IllegalStateException("main dies first");
    }
}
