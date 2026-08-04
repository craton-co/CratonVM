> Point-in-time dump of one suite run's non-passing classes. Rows are NOT
> re-verified when a fix lands, so treat every entry as "was failing when this
> was captured", not as current status.
>
> Known stale as of 2026-08-04: **all six `loader/spring-boot-jarmode-tools`
> rows below now PASS**, on HotSpot and on CratonVM with JIT and `--nojit`.
> `ExtractCommandTests`/`ExtractLayersCommandTests` were a real VM defect —
> `docs/internal/fixed-suite-bugs/springboot/jarmode-tools-extract-timestamp-preservation-FIXED.md`.
> The other four were **never CratonVM defects**: HotSpot failed them
> identically, because this fixture checkout's expected-output resources are
> CRLF while `println` on Linux emits LF —
> `docs/internal/fixed-suite-bugs/springboot/jarmode-tools-crlf-fixture-phantom-failures-FIXED.md`.
>
> That doc also covers the runner gap that let HotSpot-shared failures be filed
> as CratonVM defects, now fixed (`BOTH-FAIL`). **A HotSpot control over the
> whole 32-class residual list found 5 such rows** — the four above plus
> `ChangelogWriterTests`, which is still red here and still fails on HotSpot
> too, i.e. it is a fixture failure, not a VM one.

1 CRASH
Module	Class	Seconds
module/spring-boot-jetty	SslServerCustomizerTests	1.4
7 HANG (all at the 300s ceiling — genuinely stuck or just slow, not distinguished this round)
Module	Class
module/spring-boot-tomcat	TomcatServletWebServerFactoryTests
module/spring-boot-jetty	JettyServletWebServerFactoryTests
module/spring-boot-jooq	JooqAutoConfigurationTests
module/spring-boot-jooq	JooqFlywayDatabaseInitializationTests
module/spring-boot-http-client	JdkClientHttpRequestFactoryBuilderTests
module/spring-boot-jackson	JacksonAutoConfigurationTests
module/spring-boot-kafka	KafkaAutoConfigurationIntegrationTests
23 FAIL
Module	Class	Seconds
configuration-metadata/spring-boot-configuration-metadata-changelog-generator	ChangelogWriterTests	0.6
module/spring-boot-flyway	Flyway110AutoConfigurationTests	2.8
module/spring-boot-jdbc	HikariDataSourceConfigurationTests	30.7
module/spring-boot-mongodb	MongoReactiveAutoConfigurationTests	29.1
loader/spring-boot-jarmode-tools	ExtractCommandTests	2.0
loader/spring-boot-jarmode-tools	ExtractLayersCommandTests	2.8
loader/spring-boot-jarmode-tools	HelpCommandTests	2.4
loader/spring-boot-jarmode-tools	ListCommandTests	2.6
loader/spring-boot-jarmode-tools	ListLayersCommandTests	0.8
loader/spring-boot-jarmode-tools	ToolsJarModeTests	1.2
core/spring-boot	ApplicationPidTests	1.2
module/spring-boot-tomcat	TomcatWebServerFactoryCustomizerTests	81.5
loader/spring-boot-loader	ZipContentTests	149.2
core/spring-boot-autoconfigure	ConditionalOnCheckpointRestoreTests	3.2
module/spring-boot-gson	Gson210AutoConfigurationTests	2.0
module/spring-boot-quartz	QuartzEndpointWebIntegrationTests	295.4
core/spring-boot	NoSuchMethodFailureAnalyzerTests	9.5
module/spring-boot-health	DiskSpaceHealthIndicatorTests	3.6
module/spring-boot-http-client	JdkClientHttpConnectorBuilderTests	8.7
module/spring-boot-liquibase	Liquibase423AutoConfigurationTests	1.6
test-support/spring-boot-test-support	ModifiedClassPathExtensionOverridesParameterizedTests	2.0
test-support/spring-boot-test-support	ModifiedClassPathExtensionOverridesTests	1.8
test-support/spring-boot-test-support	ResourcesTests