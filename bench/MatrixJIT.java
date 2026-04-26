public class MatrixJIT {
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
        int n = 3;
        int[][] a = new int[n][n];
        int[][] b = new int[n][n];
        for (int i = 0; i < n; i++) {
            for (int j = 0; j < n; j++) {
                a[i][j] = i + j;
                b[i][j] = i - j;
            }
        }
        System.out.println("Calling matmul...");
        int[][] c = matmul(a, b, n);
        System.out.println("c[0][0] = " + c[0][0]);
        System.out.println("c[1][1] = " + c[1][1]);
        System.out.println("c[2][2] = " + c[2][2]);
    }
}
