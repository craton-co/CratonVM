# Compatibility testing and licensing

CratonVM is maintained by [Craton Software Company](https://craton.com.ar).
First-party source is licensed under GPL version 2 only with the Classpath
Exception 2.0. See [LICENSE](../LICENSE), [NOTICE](../NOTICE), and
[third-party notices](../THIRD-PARTY-NOTICES.md) for distribution terms and
retained attribution. Vendored dependencies are not relicensed by these notices.

The in-tree curated JDK tests are engineering regression tests. They are not
Oracle's Java Compatibility Kit, and passing them does not establish Java
certification or a right to use compatibility trademarks.

Oracle JCK access and any compatibility or trademark claims require the
applicable agreements. Obtain the suite separately through the
[OpenJDK conformance program](https://openjdk.org/groups/conformance/JckAccess/).
Do not commit or distribute licensed JCK material with this repository.

CratonVM executes authentic OpenJDK class libraries supplied by the operator.
Their distribution terms are independent of the VM's source license.
