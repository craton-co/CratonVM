import io.grpc.ManagedChannel;
import io.grpc.ManagedChannelBuilder;
import io.grpc.Status;
import io.grpc.MethodDescriptor;
public class GrpcProbe {
    public static void main(String[] args) {
        try {
            // Build a ManagedChannel (won't actually connect) — exercises
            // gRPC's transport + name-resolver registry.
            ManagedChannel ch = ManagedChannelBuilder.forAddress("localhost", 50051)
                .usePlaintext()
                .build();
            System.out.println("Channel: " + ch.getClass().getSimpleName() + " authority=" + ch.authority());
            ch.shutdownNow();
            // Verify Status code enumeration is well-formed.
            Status s = Status.OK;
            System.out.println("Status: " + s);
            if (!"OK".equals(s.getCode().name())) { System.out.println("FAIL"); System.exit(1); }
            System.out.println("OK");
        } catch (Throwable t) { t.printStackTrace(); System.exit(1); }
        System.exit(0);
    }
}
