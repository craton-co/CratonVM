// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
package cratonvm;

import java.io.IOException;
import java.lang.reflect.InvocationHandler;
import java.lang.reflect.Method;
import java.lang.reflect.Proxy;

/** End-to-end contract probe for proxy-synthesized Method.exceptionTypes. */
public final class ProxyMethodExceptionTypes {
    interface NoThrows {
        String call();
    }

    interface DeclaresIOException {
        void call() throws IOException;
    }

    private static final class CheckingHandler implements InvocationHandler {
        private final Class<?>[] expected;
        private final String result;

        CheckingHandler(Class<?>[] expected, String result) {
            this.expected = expected;
            this.result = result;
        }

        @Override
        public Object invoke(Object proxy, Method method, Object[] args) {
            Class<?>[] actual = method.getExceptionTypes();
            if (actual == null || actual.getClass() != Class[].class || actual.length != expected.length) {
                throw new AssertionError("wrong exceptionTypes array shape");
            }
            for (int i = 0; i < actual.length; i++) {
                if (actual[i] != expected[i]) {
                    throw new AssertionError("wrong declared exception at " + i);
                }
            }
            return result;
        }
    }

    public static void main(String[] args) throws Exception {
        if (run() != 1) {
            throw new AssertionError("proxy Method exceptionTypes probe failed");
        }
        System.out.println("OK proxy-method-exception-types");
    }

    public static int run() throws Exception {
        NoThrows noThrows = (NoThrows) Proxy.newProxyInstance(
                ProxyMethodExceptionTypes.class.getClassLoader(),
                new Class<?>[] {NoThrows.class},
                new CheckingHandler(new Class<?>[0], "no-throws"));
        if (!"no-throws".equals(noThrows.call())) {
            return -1;
        }

        DeclaresIOException declared = (DeclaresIOException) Proxy.newProxyInstance(
                ProxyMethodExceptionTypes.class.getClassLoader(),
                new Class<?>[] {DeclaresIOException.class},
                new CheckingHandler(new Class<?>[] {IOException.class}, null));
        declared.call();
        return 1;
    }
}