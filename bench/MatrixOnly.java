public class MatrixOnly {
    public static void main(String[] args) {
        System.out.println("Start");
        int n = 10;
        int[][] a = new int[n][n];
        System.out.println("Allocated a");
        for (int i = 0; i < n; i++) {
            for (int j = 0; j < n; j++) {
                a[i][j] = i + j;
            }
        }
        System.out.println("Filled a");
        int[][] b = new int[n][n];
        System.out.println("Allocated b");
        for (int i = 0; i < n; i++) {
            for (int j = 0; j < n; j++) {
                b[i][j] = i - j;
            }
        }
        System.out.println("Filled b");
        int[][] c = new int[n][n];
        System.out.println("Allocated c");
        for (int i = 0; i < n; i++) {
            for (int j = 0; j < n; j++) {
                int sum = 0;
                for (int k = 0; k < n; k++) {
                    sum += a[i][k] * b[k][j];
                }
                c[i][j] = sum;
            }
        }
        System.out.println("Done: c[5][5] = " + c[5][5]);
    }
}
