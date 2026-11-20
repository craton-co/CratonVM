public final class Avx512IntSumProbe {
    static long sum(int[] a) {
        long s = 0;
        for (int i = 0; i < a.length; i++) s += a[i];
        return s;
    }
    static int sumInt(int[] a) {
        int s = 0;
        for (int i = 0; i < a.length; i++) s += a[i];
        return s;
    }
    static long range(int[] a, int start, int end) {
        long s = 19;
        for (int i = start; i < end; i++) s += a[i];
        return s;
    }
    public static void main(String[] args) {
        int[] lengths = {0,1,7,8,9,15,16,17,31,32,33,255,256,257,1023,1024,1025,8191,8192,8193,65535,65536,65537,1048575,1048576,1048577};
        long checksum = 0;
        for (int n : lengths) {
            int[] a = new int[n];
            long expected = 0;
            for (int i=0; i<n; i++) { a[i] = i * 1_000_003 - 2_000_000_000; expected += a[i]; }
            for (int r=0; r<(n >= 1048575 ? 3 : 100); r++) {
                long actual = sum(a);
                if (actual != expected) throw new AssertionError("long n="+n+" "+actual+" != "+expected);
                if (sumInt(a) != (int)expected) throw new AssertionError("int n="+n);
                checksum += actual;
                for (int start=0; start<4 && start<=n; start++) {
                    long e = expected + 19;
                    for (int i=0; i<start; i++) e -= a[i];
                    if (range(a,start,n) != e) throw new AssertionError("range n="+n+" start="+start);
                }
            }
        }
        System.out.println("AVX512_INT_SUM_OK " + checksum);
    }
}
