public final class OsrConcatProbe {
    public static void main(String[] args) {
        long total = 0;
        for (int i = 0; i < 5_000_000; i++) {
            total += i;
        }
        System.out.println("first=" + total);
        for (int i = 0; i < 5_000_000; i++) {
            total += i;
        }
        System.out.println("second=" + total);
    }
}
