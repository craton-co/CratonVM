package unloadprobe;

import java.io.Serializable;

public final class LoaderUnloadPayload implements Serializable {
    public static Object retained = new LoaderUnloadPayload();
    private int value;

    public LoaderUnloadPayload() {
        value = 1;
    }

    public synchronized int hot(int n) {
        int sum = value;
        for (int i = 0; i < n; i++) {
            sum = (sum * 33) ^ i;
        }
        value = sum;
        return sum;
    }

    public static synchronized int staticHot(int n) {
        return n * 17 + 3;
    }
}
