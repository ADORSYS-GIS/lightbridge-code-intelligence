## 1. Summary

This PR changes:

**Rust Code Changes:**
- Remove insecure TLS configuration
- Add proper TLS validation using internal CA certificate via automatic system trust roots
- Update `build_tracer_provider` to use automatic system trust roots via `reqwest-rustls` feature
- Remove manual CA certificate loading code
- Simplify HTTP client creation to use rustls-native-certs
- Update TLS validation comment to reflect new approach

**Helm Chart Changes:**
- Add volumeMounts for `alloy-internal-ca` secret in control-plane container
- Add volumeMounts for `alloy-internal-ca` secret in dispatcher container
- Mount CA certificate at `/etc/otel-certs` for OTLP exporter TLS validation
- Add volume definition for `internal-ca-trust` with `alloy-internal-ca` secret

It solves:

- OpenTelemetry tracing TLS validation issue
- "OpenTelemetry layer not found" errors caused by TLS certificate validation failures
- Insecure TLS connections that bypass security best practices
- App startup failures due to CA certificate loading errors
- Misleading log messages about TLS validation behavior
- Inconsistent error handling across CA-loading failure modes
- Complex manual CA certificate management code

---

## 2. Intent

The intent of this PR is:

> Fix the OTLP tracing TLS validation issue by removing the insecure TLS workaround and implementing proper TLS validation using the internal CA certificate. The OTLP exporter was failing with TLS certificate validation errors because Tempo uses a self-signed certificate from the cluster's `self-signed-ca` ClusterIssuer. This PR enables proper TLS validation using the internal CA via automatic system trust roots, following modern Rust TLS best practices.

**Changes Summary:**

 1. **Rust Code:** Remove insecure TLS configuration from `build_tracer_provider` function in `services/observability/src/lib.rs`
 2. **Rust Code:** Add `reqwest-rustls` feature to opentelemetry-otlp for automatic system trust root loading
 3. **Rust Code:** Remove manual CA certificate loading code and `load_internal_ca_cert()` function
 4. **Rust Code:** Simplify HTTP client creation to use automatic system trust roots via rustls-native-certs
 5. **Rust Code:** Update TLS validation comment to reflect the new approach
 6. **Helm Chart:** Add volumeMounts and volumes to mount `alloy-internal-ca` secret in both control-plane and dispatcher containers

**Note:** OTLP environment variables (`OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_TRACES_SAMPLER_ARG`) are already set in `ai-helm-values`, so this PR only adds the volume mounts for TLS validation.

---

## 3. Scope

### In Scope

**Rust Code Changes:**
- Remove insecure TLS configuration from `build_tracer_provider` function
- Add `reqwest-rustls` feature to opentelemetry-otlp for automatic system trust root loading
- Remove manual CA certificate loading code from `build_tracer_provider` function
- Remove `load_internal_ca_cert()` function entirely
- Simplify HTTP client creation to use automatic system trust roots via rustls-native-certs
- Update TLS validation comment to reflect the new approach
- Remove `use std::path::PathBuf;` import (no longer needed)

**Helm Chart Changes:**
- Add volumeMounts for `alloy-internal-ca` secret in control-plane container
- Add volumeMounts for `alloy-internal-ca` secret in dispatcher container
- Mount CA certificate at `/etc/otel-certs` for OTLP exporter TLS validation
- Add volume definition for `internal-ca-trust` with `alloy-internal-ca` secret

### Out of Scope

- OTLP environment variable configuration (already set in ai-helm-values)
- Alloy collector configuration changes
- Tempo configuration changes
- Adding additional tracing spans or instrumentation
- Changing sampling ratios

---

## 4. Verification

I verified this change by:

- [x] Running cargo fmt (in lightbridge-code-intelligence repository)
- [x] Running cargo clippy (in lightbridge-code-intelligence repository)
- [x] Running cargo test (in lightbridge-code-intelligence repository)
- [ ] Running automated tests (pending CI)
- [ ] Running manual tests (pending deployment)
- [ ] Checking logs (pending deployment)
- [ ] Checking metrics (pending deployment)
- [ ] Testing error cases (pending deployment)
- [ ] Testing permissions/security behavior (pending deployment)
- [ ] Testing rollback or failure behavior, if relevant (pending deployment)

Commands run:

```bash
cd services/observability
cargo fmt
cargo clippy --all-targets --all-features
cargo test
```

Results:

```
running 3 tests
test tests::current_traceparent_is_none_without_the_otel_layer ... ok
test tests::set_remote_parent_with_none_is_a_graceful_noop ... ok
test tests::current_traceparent_round_trips_into_a_child_span_with_the_same_trace_id ... ok

test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```

**Verification of Helm Chart Changes:**

```bash
# Verify the Helm chart values are correct
kubectl get secret alloy-internal-ca -n observability -o yaml

# Expected output:
# apiVersion: v1
# kind: Secret
# metadata:
#   name: alloy-internal-ca
#   namespace: observability
# type: kubernetes.io/tls
# data:
#   tls.crt: <base64-encoded certificate>
#   tls.key: <base64-encoded key>
```

**Verification of Volume Mounts:**

```bash
# Check the Helm chart values for volumeMounts
helm get values <release-name> -n observability | grep -A 10 "control-plane"

# Expected output:
# volumeMounts:
#   - name: internal-ca-trust
#     mountPath: /etc/otel-certs
#     readOnly: true
```

---

## 5. Screenshots / Evidence

Add evidence here:

* Screenshot: [N/A - code changes only]
* Logs: [N/A - pending deployment]
* Metrics: [N/A - pending deployment]
* Recording: [N/A]

---

## 6. Risk Assessment

Risk level:

* [x] Low

Potential risks:

* System trust roots not including internal CA certificate (mitigated by mounting alloy-internal-ca secret)
* Incorrect CA certificate could cause TLS handshake failures (mitigated by proper validation)
* Secret not created before pod startup (mitigated by sync-wave in Helm chart)

Mitigation:

* The `reqwest-rustls` feature automatically loads system trust roots via rustls-native-certs
* The CA certificate is mounted as read-only for security
* Follows modern Rust TLS best practices with automatic trust root management
* The `alloy-internal-ca` secret is created by the Certificate resource with a sync-wave of `-1`, ensuring it exists before lightbridge-code-intelligence pods are deployed
* Simpler codebase with fewer manual certificate management operations

---

## 7. AI Usage Declaration

AI was used for:

* [x] Understanding existing code
* [x] Generating code
* [ ] Refactoring
* [ ] Generating tests
* [ ] Drafting documentation
* [ ] Reviewing the diff
* [ ] Not used

Human verification:

* [x] I understand every meaningful change in this PR
* [x] I checked generated code manually
* [ ] I checked generated tests manually
* [x] I removed unsupported AI assumptions
* [x] I accept responsibility for this PR

---

## 8. Reviewer Focus

Please focus your review on:

* [x] Correctness
* [ ] Architecture
* [x] Security
* [ ] Performance
* [x] Tests
* [x] Maintainability
* [ ] Product intent
* [ ] Edge cases

---

## 9. References

- [PRIVATE_INTERNAL_CA_GUIDE.md](./PRIVATE_INTERNAL_CA_GUIDE.md)
- [alloy-internal-ca Certificate](../ai-helm-values/environments/base/deps/alloy/certificate-internal-ca.yaml)
- [Grafana Internal CA Usage](../ai-helm-values/environments/base/deps/grafana/certificate-internal-ca.yaml)
- [Prometheus Redis Exporter Internal CA Usage](../ai-helm-values/environments/base/deps/prometheus-redis-exporter/certificate.yaml)
- [OTLP TLS Implementation Plan](../OTLP_TLS_IMPLEMENTATION_PLAN.md)
- [PR in lightbridge-code-intelligence](https://github.com/vymalo/lightbridge-code-intelligence/pull/new/fix-otel-tls-validation)
- [PR in ai-helm](https://github.com/ADORSYS-GIS/ai-helm/pull/new/fix-otel-tls-validation-minimal)