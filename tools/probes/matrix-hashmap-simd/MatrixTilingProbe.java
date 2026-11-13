import java.util.Arrays;
import java.util.Random;

public final class MatrixTilingProbe {
    public static void main(String[] args) {
        Random random = new Random(823514);
        long hash = 0;
        for (int n : new int[]{0,1,2,7,8,9,15,16,17,31,32,33,63,64,65,95,96,97,127,128,129}) {
            int[][] a = new int[n][n], b = new int[n][n];
            for (int i=0; i<n; i++) for (int j=0; j<n; j++) {
                a[i][j] = random.nextInt();
                b[i][j] = random.nextInt();
            }
            for (int[][] right : new int[][][]{b, a}) {
                int[][] expected = CratonBench.matmul(a, right, n);
                int[][] actual = CratonBench.matmulTiled(a, right, n);
                if (!Arrays.deepEquals(expected, actual)) throw new AssertionError("n="+n);
                for (int[] row : actual) for (int x : row) hash = hash*31+x;
            }
        }
        if (CratonBench.matrixKernel(1280, true) != 173943680) throw new AssertionError("benchmark checksum");
        System.out.println("MATRIX_TILING_OK " + hash);
    }
}
