import java.nio.charset.StandardCharsets;
import java.util.HashMap;
import java.util.Locale;
import java.util.Map;

/**
 * `java.util.Locale.toString()` is a MAP KEY in real code — Jetty's
 * `ContextHandler` keys its locale-to-encoding map on it. CratonVM's native
 * override read only the `locale_populate` side table, which the public
 * `Locale` constants never fill (they are built by real JDK bytecode that does
 * not run our `<init>`), so every one of them stringified to `""` and a
 * mapping registered under one locale answered lookups for all the others.
 *
 * Witness for `JettyServletWebServerFactoryTests.localeCharsetMappingsAreConfigured`
 * ("expected: null but was: UTF-8"). Prints one line; `bad=0` is a pass.
 */
public class LocaleToStringProbe {

	public static void main(String[] args) {
		int bad = 0;

		// Public constants — built by JDK bytecode, no side-table entry.
		bad += check("GERMAN", "de", Locale.GERMAN.toString());
		bad += check("ITALIAN", "it", Locale.ITALIAN.toString());
		bad += check("ENGLISH", "en", Locale.ENGLISH.toString());
		bad += check("GERMANY", "de_DE", Locale.GERMANY.toString());
		bad += check("ITALY", "it_IT", Locale.ITALY.toString());
		bad += check("ROOT", "", Locale.ROOT.toString());

		// Constructed locales — these already worked, keep them covered.
		bad += check("of(fr)", "fr", Locale.of("fr").toString());
		bad += check("of(fr,CA)", "fr_CA", Locale.of("fr", "CA").toString());
		bad += check("of(fr,CA,POSIX)", "fr_CA_POSIX", Locale.of("fr", "CA", "POSIX").toString());
		bad += check("of(,DE)", "_DE", Locale.of("", "DE").toString());
		bad += check("forLanguageTag(pt-BR)", "pt_BR", Locale.forLanguageTag("pt-BR").toString());

		// The failure as Jetty sees it: a String-keyed map fed by toString().
		Map<String, String> encodings = new HashMap<>();
		encodings.put(Locale.GERMAN.toString(), StandardCharsets.UTF_8.name());
		bad += check("map[GERMAN]", "UTF-8", encodings.get(Locale.GERMAN.toString()));
		bad += check("map[ITALIAN]", null, encodings.get(Locale.ITALIAN.toString()));
		bad += check("map[ENGLISH]", null, encodings.get(Locale.ENGLISH.toString()));

		System.out.println("LOCALE_TOSTRING_PROBE bad=" + bad);
	}

	private static int check(String what, String expected, String actual) {
		boolean ok = (expected == null) ? (actual == null) : expected.equals(actual);
		if (!ok) {
			System.out.println("  MISMATCH " + what + ": expected=" + expected + " actual=" + actual);
		}
		return ok ? 0 : 1;
	}
}
