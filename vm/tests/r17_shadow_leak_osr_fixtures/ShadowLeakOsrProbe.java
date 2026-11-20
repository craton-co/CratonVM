import java.nio.ByteBuffer;
import java.security.MessageDigest;

/**
 * One OSR'd loop per mode, each making a virtual call per iteration
 * (`ByteBuffer.put(byte)`, `MessageDigest.update(byte)`). 4M iterations push
 * far more than the 2 MiB shadow stack holds if any call site leaks its
 * publication; the harness asserts no `[JIT] shadow-stack overflow` line.
 */
public class ShadowLeakOsrProbe {
    public static void main(String[] args) throws Exception {
        int n = 4 * 1024 * 1024;
        String mode = args.length > 0 ? args[0] : "d";
        if (mode.equals("d")) {
            MessageDigest d = MessageDigest.getInstance("SHA-256");
            for (int j = 0; j < n; j++) {
                d.update((byte) j);
            }
            byte[] h = d.digest();
            System.out.println("digest " + (h[0] & 0xff) + " " + (h[31] & 0xff));
        } else {
            byte[] arr = new byte[n];
            ByteBuffer bb = ByteBuffer.wrap(arr);
            for (int j = 0; j < n; j++) {
                bb.put((byte) j);
            }
            long sum = 0;
            for (int j = 0; j < n; j += 4099) {
                sum += arr[j];
            }
            System.out.println("buffer " + bb.position() + " " + sum);
        }
        System.out.println("SHADOW-LEAK-OSR-OK " + mode);
    }
}
