package org.springframework.boot.context.properties.bind;

import java.util.ArrayList;
import java.util.List;

import org.springframework.core.ResolvableType;
import org.springframework.core.convert.ConversionException;
import org.springframework.core.convert.ConversionFailedException;
import org.springframework.core.convert.ConversionService;
import org.springframework.core.convert.TypeDescriptor;

/**
 * Hammers the real `BindConverter.convert(Object, TypeDescriptor, TypeDescriptor)`
 * so the JIT compiles it, with a first delegate that throws a
 * ConversionException — the path that runs the method's only exception handler
 * and branches back to the loop head, where the compiler-generated iterator
 * local is read again.
 *
 * The failure it reproduces:
 *
 *   java.lang.NullPointerException: Cannot invoke "java.util.Iterator.hasNext()"
 *   because "<local5>" is null
 *       at ...bind.BindConverter.convert(BindConverter.java:108)
 *
 * surfaced through DevToolsPooledDataSourceAutoConfigurationTests
 * .inMemoryDerbyIsShutdown as a ConfigurationPropertiesBindException on
 * `spring.datasource.hikari.validation-timeout`, and only after enough of the
 * class had run to compile `convert`. `--nojit` and
 * `CRATONVM_JIT_BISECT_SKIP=org/springframework/boot/context/properties/bind/BindConverter.convert`
 * both pass.
 *
 * Lives in the production package because `BindConverter` and its `get` factory
 * are package-private.
 */
public class BindConverterJitProbe {

    /**
     * Claims everything, then throws — like a delegate that cannot finish.
     * With `throwing=false` it declines instead, so the loop still iterates
     * past it but the method's only exception handler never runs. That is the
     * A/B that decides whether the handler is part of the mechanism.
     */
    static final class ThrowingService implements ConversionService {

        private final boolean throwing;

        ThrowingService(boolean throwing) {
            this.throwing = throwing;
        }

        @Override
        public boolean canConvert(Class<?> sourceType, Class<?> targetType) {
            return this.throwing;
        }

        @Override
        public boolean canConvert(TypeDescriptor sourceType, TypeDescriptor targetType) {
            return this.throwing;
        }

        @Override
        public <T> T convert(Object source, Class<T> targetType) {
            throw failure(source);
        }

        @Override
        public Object convert(Object source, TypeDescriptor sourceType, TypeDescriptor targetType) {
            throw failure(source);
        }

        private ConversionException failure(Object source) {
            return new ConversionFailedException(TypeDescriptor.valueOf(String.class),
                    TypeDescriptor.valueOf(Long.class), source, new IllegalArgumentException("nope"));
        }
    }

    public static void main(String[] args) {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 200_000;
        boolean throwing = args.length < 2 || !"refuse".equals(args[1]);
        System.out.println("mode              = " + (throwing ? "throw (handler runs)" : "refuse (handler never runs)"));

        List<ConversionService> delegates = new ArrayList<>();
        delegates.add(new ThrowingService(throwing));
        BindConverter converter = BindConverter.get(delegates, null);
        ResolvableType longType = ResolvableType.forClass(Long.class);

        int converted = 0;
        int expectedFailures = 0;
        int unexpected = 0;
        long firstBadAt = -1;
        String firstError = null;

        for (int i = 0; i < iterations; i++) {
            try {
                Long v = converter.convert(String.valueOf(1000 + (i & 63)), longType);
                if (v != null) {
                    converted++;
                }
            }
            catch (ConversionException ex) {
                // A delegate refusing is a legitimate outcome; the NPE is not.
                expectedFailures++;
            }
            catch (Throwable ex) {
                unexpected++;
                if (firstBadAt < 0) {
                    firstBadAt = i;
                    firstError = ex.getClass().getName() + ": " + ex.getMessage();
                }
            }
        }

        System.out.println("iterations        = " + iterations);
        System.out.println("converted         = " + converted);
        System.out.println("conversion errors = " + expectedFailures);
        System.out.println("unexpected        = " + unexpected);
        System.out.println("first bad at      = " + firstBadAt);
        System.out.println("first error       = " + firstError);
        boolean ok = unexpected == 0;
        System.out.println(ok ? "PROBE PASS" : "PROBE FAIL");
        System.exit(ok ? 0 : 1);
    }
}
