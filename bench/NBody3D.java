/**
 * Full 3D pattern matching nbody.advance() — 7 static arrays, Math.sqrt.
 */
public class NBody3D {
    static double[] x, y, z, vx, vy, vz, mass;
    static int bodyCount;

    static void advance(double dt) {
        for (int i = 0; i < bodyCount; i++) {
            for (int j = i + 1; j < bodyCount; j++) {
                double dx = x[i] - x[j];
                double dy = y[i] - y[j];
                double dz = z[i] - z[j];
                double dSquared = dx*dx + dy*dy + dz*dz;
                double distance = Math.sqrt(dSquared);
                double mag = dt / (dSquared * distance);
                vx[i] -= dx * mass[j] * mag;
                vy[i] -= dy * mass[j] * mag;
                vz[i] -= dz * mass[j] * mag;
                vx[j] += dx * mass[i] * mag;
                vy[j] += dy * mass[i] * mag;
                vz[j] += dz * mass[i] * mag;
            }
        }
        for (int i = 0; i < bodyCount; i++) {
            x[i] += dt * vx[i];
            y[i] += dt * vy[i];
            z[i] += dt * vz[i];
        }
    }

    public static void main(String[] args) {
        bodyCount = 3;
        x = new double[]{0.0, 5.0, 10.0};
        y = new double[]{0.0, 3.0, -2.0};
        z = new double[]{0.0, 1.0, 4.0};
        vx = new double[]{0.0, 0.0, 0.0};
        vy = new double[]{0.0, 0.0, 0.0};
        vz = new double[]{0.0, 0.0, 0.0};
        mass = new double[]{1.0, 2.0, 0.5};

        System.out.println("x[0]=" + x[0] + " y[0]=" + y[0] + " z[0]=" + z[0]);
        for (int step = 0; step < 5000; step++) {
            advance(0.01);
        }
        System.out.println("After 5000 steps:");
        System.out.println("x[0]=" + x[0] + " y[0]=" + y[0] + " z[0]=" + z[0]);
        System.out.println("vx[0]=" + vx[0] + " vy[0]=" + vy[0] + " vz[0]=" + vz[0]);
    }
}
