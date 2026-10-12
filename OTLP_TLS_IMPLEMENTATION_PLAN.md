# OTLP TLS Implementation Plan - Using rustls Native Trust Roots

## Executive Summary

This document outlines the implementation plan for enabling secure OTLP tracing between the control-plane and dispatcher services and the Alloy collector, using the `reqwest-rustls` feature to automatically load system trust roots via `rustls-native-certs`.

---

## Current State

### What Exists

1. **Environment Variables** (already set in ai-helm-values):
   ```yaml
   env:
     - name: OTEL_EXPORTER_OTLP_ENDPOINT
       value: "https://alloy.observability.svc.cluster.local:4318"
     - name: OTEL_TRACES_SAMPLER_ARG
       value: "0.1"
   ```

2. **Certificate Resource** (exists in ai-helm):
   ```yaml
   # ai-helm-values/environments/base/deps/alloy/certificate-internal-ca.yaml
   apiVersion: cert-manager.io/v1
   kind: Certificate
   metadata:
     name: alloy-internal-ca
     namespace: observability
   spec:
     secretName: alloy-internal-ca
     issuerRef:
       name: self-signed-ca
   ```
   This creates a Secret with the Root CA certificate at `/etc/otel-certs/ca.crt`.

3. **Manual CA Loading Code** (in `services/observability/src/lib.rs`):
   ```rust
   // ❌ Current approach - overly complex
   fn load_internal_ca_cert() -> anyhow::Result<PathBuf> {
       let ca_cert_path = PathBuf::from("/etc/otel-certs/ca.crt");
       // ... manual file reading
   }

   let ca_cert = std::fs::read_to_string(&ca_cert_path)?;
   let http_client = reqwest::Client::builder()
       .danger_accept_invalid_certs(false)
       .add_root_certificate(reqwest::Certificate::from_pem(ca_cert.as_bytes()))?
       .build()?;
   ```

### What's Missing

1. **Feature Flag**: `reqwest-rustls` not enabled in `opentelemetry-otlp`
2. **Simplified Code**: Manual CA loading should be removed
3. **Volume Mounts**: Currently required (but shouldn't be with system trust roots)

---

## How rustls Loads Certificates Automatically

### The Mechanism

`rustls-native-certs` is a crate that provides a function to load the system's trust store into a `rustls::RootCertStore`:

```rust
use rustls_native_certs::load_native_certs;

fn load_system_trust_roots() -> Result<rustls::RootCertStore, Box<dyn std::error::Error>> {
    let mut roots = rustls::RootCertStore::empty();
    let certs = load_native_certs()?;
    for cert in certs {
        roots.add(cert)?;
    }
    Ok(roots)
}
```

### How `reqwest-rustls` Integrates

When you enable the `reqwest-rustls` feature in `opentelemetry-otlp`:

1. **Dependency Chain**:
   ```
   opentelemetry-otlp (reqwest-rustls feature)
       ↓
   reqwest (with rustls)
       ↓
   rustls (crypto provider)
       ↓
   rustls-native-certs (loads system trust store)
   ```

2. **Automatic Loading**:
   - `reqwest-rustls` automatically calls `rustls-native_certs::load_native_certs()`
   - This loads certificates from:
     - System certificate stores (Linux: `/etc/ssl/certs/ca-certificates.crt`)
     - Mozilla's public trust roots
     - Your cluster's internal CA (if it's in the system trust store)

3. **No Manual Configuration Needed**:
   ```rust
   // ✅ With reqwest-rustls - automatic!
   let exporter = SpanExporter::builder()
       .with_http()
       .with_endpoint("https://alloy.observability.svc.cluster.local:4318")
       .with_protocol(Protocol::HttpBinary)
       .build()?;  // Uses system trust roots automatically
   ```

### Why This Works for Internal CA

The internal CA (`self-signed-ca`) is a **self-signed cluster-scoped ClusterIssuer**. When you mount the `alloy-internal-ca` secret with the CA certificate into the pod, the certificate becomes part of the pod's filesystem. However, `reqwest-rustls` loads from the **system trust store**, not from the filesystem.

**Critical Point**: For `reqwest-rustls` to trust the internal CA, the CA certificate must be in the system trust store, not just mounted.

### Two Approaches

#### Approach 1: System Trust Store (Recommended)

**Pros:**
- No manual CA loading code
- No volume mounts required
- Standard approach
- Easy to maintain

**Cons:**
- Requires CA to be in system trust store
- May need to update system certificates periodically

**Implementation:**
```toml
# Cargo.toml
opentelemetry-otlp = { version = "0.32", default-features = false, features = ["http-proto", "reqwest-client", "reqwest-rustls"] }
```

#### Approach 2: Manual Mount (Current PR)

**Pros:**
- Explicit control over which CA is trusted
- Works even if CA is not in system trust store
- Follows existing pattern (Grafana, Redis exporter)

**Cons:**
- Requires volume mounts
- Requires manual CA loading code
- More complex

**Implementation:**
```yaml
# Helm values
volumes:
  - name: internal-ca-trust
    secret:
      secretName: alloy-internal-ca

volumeMounts:
  - name: internal-ca-trust
    mountPath: /etc/otel-certs
    readOnly: true
```

```rust
// Rust code
let ca_cert_path = PathBuf::from("/etc/otel-certs/ca.crt");
let ca_cert = std::fs::read_to_string(&ca_cert_path)?;
let http_client = reqwest::Client::builder()
    .danger_accept_invalid_certs(false)
    .add_root_certificate(reqwest::Certificate::from_pem(ca_cert.as_bytes()))?
    .build()?;
```

---

## PR Analysis: `fix-otel-tls-validation-minimal`

### What the PR Does

The PR in `../ai-helm` adds volume mounts for the internal CA certificate:

```yaml
# charts/lightbridge-code-intelligence/values.yaml
lci:
  control-plane:
    container:
      volumeMounts:
        - name: internal-ca-trust
          mountPath: /etc/otel-certs
          readOnly: true
      volumes:
        - name: internal-ca-trust
          secret:
            secretName: alloy-internal-ca

  dispatcher:
    container:
      volumeMounts:
        - name: internal-ca-trust
          mountPath: /etc/otel-certs
          readOnly: true
      volumes:
        - name: internal-ca-trust
          secret:
            secretName: alloy-internal-ca
```

### Will It Work?

**Yes, but with caveats:**

1. **✅ The PR is correct for the current implementation**
   - It adds the required volume mounts
   - It follows the existing pattern (Grafana, Redis exporter)
   - It enables TLS validation with the internal CA

2. **❌ However, it's not the optimal approach**
   - Requires manual CA loading code in Rust
   - Requires volume mounts in Helm
   - More complex than necessary

3. **⚠️ The PR assumes manual CA loading exists**
   - The PR description says: "Rust code changes to enable proper TLS validation using the internal CA certificate are in a separate PR in the lightbridge-code-intelligence repository"
   - This means the Rust code must be updated to use the mounted CA certificate

### Recommendation

**Option A: Keep the PR as-is (Approach 2)**

If you want to use the current manual CA loading approach:

1. ✅ Keep the PR in ai-helm (volume mounts)
2. ✅ Update Rust code to use `/etc/otel-certs/ca.crt`
3. ✅ Remove `reqwest-rustls` feature (not needed)

**Option B: Simplify with reqwest-rustls (Approach 1)**

If you want to use the automatic system trust roots:

1. ❌ **Do NOT merge the PR** (volume mounts not needed)
2. ✅ Update `Cargo.toml` to add `reqwest-rustls` feature
3. ✅ Simplify Rust code (remove manual CA loading)
4. ✅ Remove volume mounts from Helm values
5. ✅ Remove `alloy-internal-ca` Certificate resource (optional)

---

## Implementation Plan

### Phase 1: Update Dependencies

**File:** `Cargo.toml` (root)

```toml
# Change from:
opentelemetry-otlp = { version = "0.32", default-features = false, features = ["http-proto", "reqwest-client"] }

# To:
opentelemetry-otlp = { version = "0.32", default-features = false, features = ["http-proto", "reqwest-client", "reqwest-rustls"] }
```

**Verification:**
```bash
cargo tree | grep reqwest
# Should show: reqwest 0.13.x (features: rustls, reqwest-rustls)
```

---

### Phase 2: Simplify Observability Crate

**File:** `services/observability/src/lib.rs`

**Remove:**
```rust
// ❌ DELETE - No longer needed
use std::path::PathBuf;

fn load_internal_ca_cert() -> anyhow::Result<PathBuf> {
    let ca_cert_path = PathBuf::from("/etc/otel-certs/ca.crt");
    // ... (entire function)
}
```

**Simplify `build_tracer_provider()`:**
```rust
// ✅ New simplified version
fn build_tracer_provider(service_name: &str, endpoint: &str) -> anyhow::Result<SdkTracerProvider> {
    use opentelemetry_otlp::{Protocol, SpanExporter, WithExportConfig, WithHttpConfig};
    use opentelemetry_sdk::runtime::Tokio;
    use opentelemetry_sdk::trace::span_processor_with_async_runtime::BatchSpanProcessor;

    let ratio: f64 = std::env::var("OTEL_TRACES_SAMPLER_ARG")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0.1);

    // `endpoint` is the configured BASE url; append the OTLP/HTTP traces path ourselves
    let traces_endpoint = format!("{}/v1/traces", endpoint.trim_end_matches('/'));

    // Build exporter with built-in reqwest client (uses system trust roots via reqwest-rustls)
    let exporter = SpanExporter::builder()
        .with_http()
        .with_endpoint(traces_endpoint)
        .with_protocol(Protocol::HttpBinary)
        .build()?;

    let processor = BatchSpanProcessor::builder(exporter, Tokio).build();

    let resource = opentelemetry_sdk::Resource::builder()
        .with_service_name(service_name.to_string())
        .build();

    Ok(SdkTracerProvider::builder()
        .with_span_processor(processor)
        .with_sampler(Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(
            ratio,
        ))))
        .with_resource(resource)
        .build())
}
```

---

### Phase 3: Update Helm Values

**File:** `charts/lightbridge-code-intelligence/values.yaml` (in ai-helm)

**Remove:**
```yaml
# ❌ DELETE - No longer needed
lci:
  control-plane:
    container:
      volumeMounts:
        - name: internal-ca-trust
          mountPath: /etc/otel-certs
          readOnly: true
      volumes:
        - name: internal-ca-trust
          secret:
            secretName: alloy-internal-ca

  dispatcher:
    container:
      volumeMounts:
        - name: internal-ca-trust
          mountPath: /etc/otel-certs
          readOnly: true
      volumes:
        - name: internal-ca-trust
          secret:
            secretName: alloy-internal-ca
```

**Keep:**
```yaml
# ✅ Keep - Environment variables are still needed
lci:
  control-plane:
    env:
      - name: OTEL_EXPORTER_OTLP_ENDPOINT
        value: "https://alloy.observability.svc.cluster.local:4318"
      - name: OTEL_TRACES_SAMPLER_ARG
        value: "0.1"

  dispatcher:
    env:
      - name: OTEL_EXPORTER_OTLP_ENDPOINT
        value: "https://alloy.observability.svc.cluster.local:4318"
      - name: OTEL_TRACES_SAMPLER_ARG
        value: "0.1"
```

---

### Phase 4: Remove Certificate Resource (Optional)

**File:** `ai-helm-values/environments/base/deps/alloy/certificate-internal-ca.yaml`

**Option A: Keep for Documentation**
```yaml
# Keep as documentation of the internal CA
apiVersion: cert-manager.io/v1
kind: Certificate
metadata:
  name: alloy-internal-ca
  namespace: observability
spec:
  secretName: alloy-internal-ca
  issuerRef:
    name: self-signed-ca
```

**Option B: Remove Entirely**
```yaml
# ❌ DELETE - No longer needed with reqwest-rustls
```

---

### Phase 5: Verify Implementation

```bash
# 1. Build the project
cargo build

# 2. Check dependencies
cargo tree | grep -E "reqwest|rustls"

# 3. Run tests
cargo test

# 4. Deploy and verify
helm upgrade --install control-plane ./charts/lightbridge-code-intelligence \
  --namespace platform \
  --values ./values.yaml

# 5. Check logs
kubectl logs -n platform -l app.kubernetes.io/name=control-plane --tail=100 | grep -i otel

# 6. Verify traces flow
kubectl logs -n observability -l app.kubernetes.io/name=alloy --tail=100
```

---

## Architecture Diagram

### With Manual CA Loading (Current PR)

```
┌─────────────────────────────────────────────────────────────┐
│  Control-Plane & Dispatcher Pods                             │
│  - Mount alloy-internal-ca secret                          │
│  - CA cert at /etc/otel-certs/ca.crt                        │
└──────────────────────┬──────────────────────────────────────┘
                        │ Manual file read
                        ↓
┌─────────────────────────────────────────────────────────────┐
│  Rust Code                                                   │
│  - load_internal_ca_cert() reads /etc/otel-certs/ca.crt    │
│  - Creates reqwest client with CA                           │
└──────────────────────┬──────────────────────────────────────┘
                        │ TLS validated against CA
                        ↓
┌─────────────────────────────────────────────────────────────┐
│  Alloy Collector                                             │
│  - Receives OTLP traces                                      │
└─────────────────────────────────────────────────────────────┘
```

### With reqwest-rustls (Recommended)

```
┌─────────────────────────────────────────────────────────────┐
│  Control-Plane & Dispatcher Pods                             │
│  - No volume mounts needed                                   │
│  - System trust roots loaded automatically                   │
└──────────────────────┬──────────────────────────────────────┘
                        │ Automatic
                        ↓
┌─────────────────────────────────────────────────────────────┐
│  rustls-native-certs                                        │
│  - Loads system trust store                                  │
│  - Includes internal CA if in system trust store            │
└──────────────────────┬──────────────────────────────────────┘
                        │ TLS validated against system trust
                        ↓
┌─────────────────────────────────────────────────────────────┐
│  Alloy Collector                                             │
│  - Receives OTLP traces                                      │
└─────────────────────────────────────────────────────────────┘
```

---

## Risk Assessment

### With Manual CA Loading (Current PR)

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| CA certificate not found | Low | High | Error handling in Rust code |
| Incorrect CA certificate | Low | High | Proper validation |
| Volume mount fails | Very Low | Medium | Standard Kubernetes pattern |

### With reqwest-rustls (Recommended)

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| CA not in system trust store | Medium | High | Mount CA into system trust store |
| System trust store outdated | Low | Medium | Periodic updates |
| No volume mounts | N/A | N/A | Simpler deployment |

---

## Decision Matrix

| Factor | Manual CA Loading | reqwest-rustls |
|--------|-------------------|----------------|
| **Code Complexity** | High (manual file read) | Low (automatic) |
| **Helm Complexity** | High (volume mounts) | Low (no mounts) |
| **Maintenance** | Medium (manual updates) | Low (automatic) |
| **Flexibility** | High (explicit control) | Medium (system trust) |
| **Security** | High (explicit) | High (system trust) |
| **Performance** | Low (file I/O) | High (automatic) |
| **Recommended for** | Production with strict control | Development / Production with system trust |

---

## Next Steps

### If You Choose Manual CA Loading (Option A)

1. ✅ Keep the PR in ai-helm
2. ✅ Update Rust code to use `/etc/otel-certs/ca.crt`
3. ✅ Test the implementation

### If You Choose reqwest-rustls (Option B)

1. ❌ **Do NOT merge the PR** in ai-helm
2. ✅ Update `Cargo.toml` to add `reqwest-rustls` feature
3. ✅ Simplify Rust code
4. ✅ Remove volume mounts from Helm values
5. ✅ Remove or keep `alloy-internal-ca` Certificate resource (optional)
6. ✅ Test the implementation

---

## References

- [opentelemetry-otlp Documentation](https://docs.rs/opentelemetry-otlp/latest/opentelemetry_otlp/)
- [reqwest-rustls Feature](https://docs.rs/opentelemetry-otlp/latest/opentelemetry_otlp/#feature-flags)
- [rustls-native-certs Crate](https://docs.rs/rustls-native-certs/latest/rustls_native_certs/)
- [PRIVATE_INTERNAL_CA_GUIDE.md](../ai-helm/PRIVATE_INTERNAL_CA_GUIDE.md)
- [OTLP TLS Implementation Plan](./OTLP_TLS_IMPLEMENTATION_PLAN.md)
- [PR in ai-helm](https://github.com/ADORSYS-GIS/ai-helm/pull/new/fix-otel-tls-validation-minimal)