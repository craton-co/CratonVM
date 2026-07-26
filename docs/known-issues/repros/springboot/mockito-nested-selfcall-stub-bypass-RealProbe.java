package org.springframework.boot.micrometer.metrics.autoconfigure.export.otlp;

import org.springframework.boot.micrometer.metrics.autoconfigure.export.otlp.OtlpMetricsExportAutoConfiguration.PropertiesOtlpMetricsConnectionDetails;
import org.springframework.boot.opentelemetry.autoconfigure.OpenTelemetryProperties;
import org.springframework.mock.env.MockEnvironment;

import static org.mockito.BDDMockito.given;
import static org.mockito.Mockito.spy;

public class RealProbe {
    public static void main(String[] args) throws Exception {
        OtlpMetricsProperties properties = new OtlpMetricsProperties();
        OpenTelemetryProperties openTelemetryProperties = new OpenTelemetryProperties();
        MockEnvironment environment = new MockEnvironment();
        OtlpMetricsConnectionDetails connectionDetails = new PropertiesOtlpMetricsConnectionDetails(properties, null);

        OtlpMetricsPropertiesConfigAdapter adapter = new OtlpMetricsPropertiesConfigAdapter(properties,
                openTelemetryProperties, connectionDetails, environment);
        OtlpMetricsPropertiesConfigAdapter spyAdapter = spy(adapter);
        given(spyAdapter.get("management.otlp.metrics.export.url")).willReturn("https://my-endpoint/v1/metrics");
        System.out.println("RESULT url() = " + spyAdapter.url());
        try {
            org.mockito.Mockito.verify(spyAdapter, org.mockito.Mockito.atLeastOnce())
                    .get("management.otlp.metrics.export.url");
            System.out.println("VERIFY: get(key) WAS recorded as a mock invocation (via url()'s nested call)");
        } catch (Throwable t) {
            System.out.println("VERIFY FAILED (get() was never recorded via url()): " + t.getClass().getSimpleName());
        }
    }
}
