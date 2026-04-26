public class MatrixMid {
    static int[][] matmul(int[][] a, int[][] b, int n) {
        int[][] c = new int[n][n];
        for (int i = 0; i < n; i++) {
            for (int j = 0; j < n; j++) {
                int sum = 0;
                for (int k = 0; k < n; k++) {
                    sum += a[i][k] * b[k][j];
                }
                c[i][j] = sum;
            }
        }
        return c;
    }

    public static void main(String[] args) {
        int n = 50;
        System.out.println("Allocating " + n + "x" + n + " matrices...");
        int[][] a = new int[n][n];
        int[][] b = new int[n][n];
        for (int i = 0; i < n; i++) {
            for (int j = 0; j < n; j++) {
                a[i][j] = i + j;
                b[i][j] = i - j;
            }
        }
        System.out.println("Multiplying...");
        long t0 = System.currentTimeMillis();
        int[][] c = matmul(a, b, n);
        long elapsed = System.currentTimeMillis() - t0;
        System.out.println("Done: " + elapsed + " ms [" + c[25][25] + "]");
    }
}
