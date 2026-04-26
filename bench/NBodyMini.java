/**
 * Minimal N-Body advance pattern: two static double arrays, nested loops,
 * dup2 + daload + dsub + dmul + dastore.
 */
public class NBodyMini {
    static double[] x;
    static double[] vx;
    static int bodyCount;

    static void advance(double dt) {
        for (int i = 0; i < bodyCount; i++) {
            for (int j = i + 1; j < bodyCount; j++) {
                double dx = x[i] - x[j];
                double dist = dx * dx;
                // Simplified: no sqrt, just use dist directly
                double mag = dt / dist;
                vx[i] -= dx * mag;
                vx[j] += dx * mag;
            }
        }
        for (int i = 0; i < bodyCount; i++) {
            x[i] += dt * vx[i];
        }
    }

    public static void main(String[] args) {
        bodyCount = 3;
        x = new double[3];
        vx = new double[3];
        x[0] = 0.0; x[1] = 5.0; x[2] = 10.0;
        vx[0] = 0.0; vx[1] = 0.0; vx[2] = 0.0;

        System.out.println("x[0]=" + x[0] + " x[1]=" + x[1] + " x[2]=" + x[2]);
        System.out.println("vx[0]=" + vx[0] + " vx[1]=" + vx[1] + " vx[2]=" + vx[2]);

        for (int step = 0; step < 5000; step++) {
            advance(0.01);
        }

        System.out.println("After 5000 steps:");
        System.out.println("x[0]=" + x[0] + " x[1]=" + x[1] + " x[2]=" + x[2]);
        System.out.println("vx[0]=" + vx[0] + " vx[1]=" + vx[1] + " vx[2]=" + vx[2]);
    }
}
