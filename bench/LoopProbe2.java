public class LoopProbe2 {
    static long sum(int start, int count) {
        long s = 0;
        for (int i = start; i < count; i++) {
            s += i;
        }
        return s;
    }

    public static void main(String[] args) {
        int start = Integer.parseInt(args[0]);
        int count = Integer.parseInt(args[1]);
        long result = sum(start, count);
        System.out.println("result=" + result);
    }
}
