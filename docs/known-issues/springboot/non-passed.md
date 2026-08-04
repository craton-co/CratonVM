> Point-in-time dump of one suite run's non-passing classes. Rows are NOT
> re-verified when a fix lands, so treat every entry as "was failing when this
> was captured", not as current status.
>
> Known stale as of 2026-08-04:
> `loader/spring-boot-jarmode-tools` `ExtractCommandTests` (22/22) and
> `ExtractLayersCommandTests` (6/6) now PASS with JIT and `--nojit` — see
> `docs/internal/fixed-suite-bugs/springboot/jarmode-tools-extract-timestamp-preservation-FIXED.md`.
> The other four `jarmode-tools` rows below are still red and untriaged.

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