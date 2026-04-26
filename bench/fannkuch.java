/* The Computer Language Benchmarks Game
   Fannkuch-Redux - permutation and pancake flipping
*/
public final class fannkuch {

    static int[] fannkuchRedux(int n) {
        int maxFlips = 0;
        int checksum = 0;
        int[] perm = new int[n];
        int[] perm1 = new int[n];
        int[] count = new int[n];
        for(int i=0; i<n; i++) perm1[i] = i;

        int r = n;
        while(true) {
            while(r != 1){ count[r-1] = r; r--; }
            for(int i=0; i<n; i++) perm[i] = perm1[i];
            int flips = 0;
            int k;
            while((k=perm[0]) != 0){
                int k2 = (k+1) >> 1;
                for(int i=0; i<k2; i++){
                    int t = perm[i]; perm[i] = perm[k-i]; perm[k-i] = t;
                }
                flips++;
            }
            if(flips > maxFlips) maxFlips = flips;
            checksum += ((count[1] & 1) == 0) ? flips : -flips;

            while(true){
                if(r == n) {
                    int[] result = new int[2];
                    result[0] = checksum;
                    result[1] = maxFlips;
                    return result;
                }
                int perm0 = perm1[0];
                int i = 0;
                while(i < r){
                    int j = i + 1;
                    perm1[i] = perm1[j];
                    i = j;
                }
                perm1[r] = perm0;
                count[r] = count[r] - 1;
                if(count[r] > 0) break;
                r++;
            }
        }
    }

    public static void main(String[] args) {
        int n = 11;
        fannkuchRedux(9); // warmup

        long t0 = System.currentTimeMillis();
        int[] result = fannkuchRedux(n);
        long elapsed = System.currentTimeMillis() - t0;
        System.out.println(result[0]);
        System.out.println("Pfannkuchen("+n+") = " + result[1]);
        System.out.println("=== Fannkuch-Redux (n="+n+") ===");
        System.out.println("Time: " + elapsed + " ms");
    }
}
