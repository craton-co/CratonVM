import java.net.InetAddress;
import java.net.InetSocketAddress;

/** Regression for same-named SAM/default overload dispatch on a lambda proxy. */
public final class RLambdaDefaultOverload {

	@FunctionalInterface
	interface AddressFilter {
		boolean matches(InetAddress address);

		default boolean matches(InetSocketAddress address) {
			if (address == null) {
				throw new IllegalArgumentException("address must not be null");
			}
			return matches(address.getAddress());
		}
	}

	public static void main(String[] args) throws Exception {
		AddressFilter filter = (address) -> address != null && address.isLoopbackAddress();
		boolean rejectedNull = false;
		try {
			filter.matches((InetSocketAddress) null);
		}
		catch (IllegalArgumentException ex) {
			rejectedNull = "address must not be null".equals(ex.getMessage());
		}
		check(rejectedNull, "default overload must reject null before the lambda body");
		check(filter.matches(new InetSocketAddress(InetAddress.getLoopbackAddress(), 8080)),
				"default overload must delegate to the lambda SAM");
		System.out.println("CK lambda-default-overload=2");
		System.out.println("PASS RLambdaDefaultOverload (2 checks)");
	}

	private static void check(boolean condition, String message) {
		if (!condition) {
			throw new AssertionError(message);
		}
	}
}
