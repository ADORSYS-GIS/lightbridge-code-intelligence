# Private Internal CA Certificate Guide for OTEL Traces Exporting to Alloy Tempo

## Table of Contents

1. [What is a Private Internal CA?](#what-is-a-private-internal-ca)
2. [Why Use Private Internal CAs for OTEL?](#why-use-private-internal-cas-for-otel)
3. [Architecture in This Repository](#architecture-in-this-repository)
4. [OTEL Traces Export Pipeline](#otel-traces-export-pipeline)
5. [Practical Implementation Examples](#practical-implementation-examples)
6. [Troubleshooting & Debugging](#troubleshooting--debugging)
7. [Security Best Practices](#security-best-practices)

---

## What is a Private Internal CA?

A **Private Internal Certificate Authority (CA)** is a self-signed certificate authority that issues certificates for services within a private network (cluster, VPC, or internal network). Unlike public CAs (like Let's Encrypt), private CAs:

- **Are self-signed** - No external authority needed
- **Are cluster-scoped** - Trusted only within your Kubernetes cluster
- **Never expire** - Self-signed certificates don't have expiration dates
- **Are private** - Only accessible to services within your network

### Key Characteristics

```yaml
# Example: Self-Signed CA ClusterIssuer
apiVersion: cert-manager.io/v1
kind: ClusterIssuer
metadata:
  name: self-signed-ca
  namespace: cert-manager
spec:
  selfSigned: {}
```

**Properties:**
- **Root CA**: The self-signed certificate authority
- **Intermediate CA**: Optional - issues certificates for services
- **Leaf Certificates**: Individual service certificates signed by the CA
- **Trust Bundle**: CA certificate(s) that clients trust

---

## Why Use Private Internal CAs for OTEL?

### Benefits for OTEL Traces

1. **Secure Internal Communication**
   - Encrypt traces between applications and collectors
   - Prevent eavesdropping on sensitive trace data

2. **No External Dependencies**
   - No need for Let's Encrypt or external CAs
   - Works offline or in air-gapped environments

3. **Simplified Certificate Management**
   - Self-signed certificates don't expire
   - No need to renew certificates

4. **Fine-Grained Control**
   - Control which services can issue certificates
   - Control which services trust which CAs

### Use Cases in This Repository

```yaml
# Internal plane communication
gateway:
  internal:
    enabled: true
    hostname: core-gateway-internal.envoy-gateway-system.svc.cluster.local
    issuer: self-signed-ca  # Uses private internal CA
```

---

## Architecture in This Repository

### Private Internal CA Setup

The repository uses a **self-signed Root CA** (`self-signed-ca`) that issues certificates for internal services:

```
┌─────────────────────────────────────┐
│    Root CA (self-signed-ca)         │
│    - Self-signed ClusterIssuer      │
│    - Never expires                  │
│    - Cluster-scoped                 │
└──────────────┬──────────────────────┘
               │ issues
               ↓
┌─────────────────────────────────────┐
│  Intermediate CA Certificate        │
│  - Signed by Root CA                │
│  - Used for service certificates    │
└──────────────┬──────────────────────┘
               │ issues
               ↓
┌─────────────────────────────────────┐
│  Service Certificates (Leaf)        │
│  - Signed by Intermediate CA        │
│  - Used for TLS termination         │
│  - Internal plane only              │
└─────────────────────────────────────┘
```

### Certificate Components

#### 1. Root CA (Self-Signed)

```yaml
# charts/core-gateway/templates/acme-issuer.yaml (example structure)
apiVersion: cert-manager.io/v1
kind: ClusterIssuer
metadata:
  name: self-signed-ca
  namespace: cert-manager
spec:
  selfSigned: {}
```

**What this creates:**
- A self-signed Root CA certificate
- Cluster-scoped (trusted across all namespaces)
- Never expires (self-signed)
- Used as the trust anchor for all internal services

#### 2. Internal CA Trust Bundle

```yaml
# charts/librechat-app/templates/certificate-internal-ca.yaml
apiVersion: cert-manager.io/v1
kind: Certificate
metadata:
  name: internal-ca-trust-bundle
  namespace: librechat
spec:
  secretName: internal-ca-trust-bundle
  commonName: internal-ca-trust-bundle
  dnsNames:
    - internal-ca-trust-bundle
  issuerRef:
    name: self-signed-ca
    kind: ClusterIssuer
```

**What this creates:**
- A Secret containing the Root CA certificate (`ca.crt`)
- Mounted by clients to trust the internal CA
- Used for TLS verification of internal services

#### 3. Service Certificate

```yaml
# charts/core-gateway/templates/certificate-internal.yaml
apiVersion: cert-manager.io/v1
kind: Certificate
metadata:
  name: core-gateway-internal-tls
  namespace: envoy-gateway-system
spec:
  secretName: core-gateway-internal-tls
  dnsNames:
    - core-gateway-internal.envoy-gateway-system.svc.cluster.local
  issuerRef:
    name: self-signed-ca
    kind: ClusterIssuer
```

**What this creates:**
- A TLS certificate for the internal gateway
- Signed by the Root CA
- Valid for in-cluster communication only

---

## OTEL Traces Export Pipeline

### Architecture with Private Internal CA

```
┌─────────────────┐
│   Application   │
│   (OTEL SDK)    │
│                 │
│  ┌───────────┐  │
│  │ TLS Client│  │
│  │ (trust CA)│  │
│  └─────┬─────┘  │
└────────┼────────┘
         │ OTLP (gRPC/HTTP)
         │ TLS: client-cert + ca.crt
         │
┌────────▼────────┐
│  OpenTelemetry  │
│  Collector      │
│  (otel.yaml)    │
│                 │
│  ┌───────────┐  │
│  │ TLS Server│  │
│  │ (verify CA)│  │
│  └─────┬─────┘  │
└────────┼────────┘
         │ OTLP
         │ TLS: client-cert + ca.crt
         │
┌────────▼────────┐
│   Grafana Alloy │
│   (Collector)   │
│                 │
│  ┌───────────┐  │
│  │ TLS Server│  │
│  │ (verify CA)│  │
│  └─────┬─────┘  │
└────────┼────────┘
         │ OTLP
         │ TLS: client-cert + ca.crt
         │
┌────────▼────────┐
│    Tempo        │
│   (Trace Store) │
│                 │
│  ┌───────────┐  │
│  │ TLS Server│  │
│  │ (verify CA)│  │
│  └───────────┘  │
└─────────────────┘
```

### Configuration Files

#### 1. OpenTelemetry Collector with Private Internal CA

```yaml
# charts/core-gateway/templates/otel.yaml
apiVersion: opentelemetry.io/v1beta1
kind: OpenTelemetryCollector
metadata:
  name: {{ include "core-gateway.fullname" . }}-traces
  namespace: {{ .Release.Namespace }}
spec:
  mode: deployment
  config:
    receivers:
      otlp:
        protocols:
          grpc:
            endpoint: 0.0.0.0:4317
            tls:
              insecure: false
              ca_file: /etc/otel-certs/ca.crt  # Root CA for verification
          http:
            endpoint: 0.0.0.0:4318
            tls:
              insecure: false
              ca_file: /etc/otel-certs/ca.crt

    processors:
      memory_limiter:
        check_interval: 1s
        limit_mib: 400
        spike_limit_mib: 100
      batch:
        send_batch_size: 512
        timeout: 5s

    exporters:
      # Forward traces to Alloy with TLS
      otlp/alloy:
        endpoint: "alloy.observability.svc.cluster.local:4317"
        tls:
          insecure: false
          ca_file: /etc/otel-certs/ca.crt  # Verify Alloy's certificate
          cert_file: /etc/otel-certs/client.crt  # Client certificate
          key_file: /etc/otel-certs/client.key  # Client private key

    service:
      pipelines:
        traces:
          receivers:
            - otlp
          processors:
            - memory_limiter
            - batch
          exporters:
            - otlp/alloy
```

#### 2. Alloy Configuration with Private Internal CA

```yaml
# Alloy receives OTLP traces with TLS and forwards to Tempo
receivers:
  otlp:
    protocols:
      grpc:
        endpoint: 0.0.0.0:4317
        tls:
          insecure: false
          ca_file: /etc/otel-certs/ca.crt
      http:
        endpoint: 0.0.0.0:4318
        tls:
          insecure: false
          ca_file: /etc/otel-certs/ca.crt

processors:
  memory_limiter:
    check_interval: 1s
    limit_mib: 400
    spike_limit_mib: 100
  batch:
    send_batch_size: 512
    timeout: 5s

exporters:
  tempo:
    endpoint: tempo:3200
    tls:
      insecure: false
      ca_file: /etc/otel-certs/ca.crt

service:
  pipelines:
    traces:
      receivers:
        - otlp
      processors:
        - memory_limiter
        - batch
      exporters:
        - tempo
```

---

## Practical Implementation Examples

### Example 1: Application with Private Internal CA Trust

```yaml
# Node.js application with OTEL SDK
apiVersion: apps/v1
kind: Deployment
metadata:
  name: my-app
  namespace: my-namespace
spec:
  template:
    spec:
      containers:
        - name: my-app
          image: my-app:latest
          env:
            # Configure OTEL endpoint with TLS
            - name: OTEL_EXPORTER_OTLP_ENDPOINT
              value: "https://core-gateway-internal.envoy-gateway-system.svc.cluster.local:4317"
            - name: OTEL_EXPORTER_OTLP_CERTIFICATE
              value: "/etc/otel-certs/ca.crt"
            - name: OTEL_EXPORTER_OTLP_HEADERS
              value: "x-internal-ca-trust=true"
          volumeMounts:
            - name: internal-ca
              mountPath: /etc/otel-certs
              readOnly: true
      volumes:
        - name: internal-ca
          secret:
            secretName: internal-ca-trust-bundle
```

### Example 2: Node.js OTEL SDK Configuration

```javascript
// Node.js application with private internal CA
const { NodeTracerProvider } = require('@opentelemetry/sdk-node');
const { OTLPTraceExporter } = require('@opentelemetry/exporter-trace-otlp-grpc');
const { Resource } = require('@opentelemetry/resources');
const { SemanticResourceAttributes } = require('@opentelemetry/semantic-conventions');

// Create provider
const provider = new NodeTracerProvider({
  resource: new Resource({
    [SemanticResourceAttributes.SERVICE_NAME]: 'my-app',
    [SemanticResourceAttributes.SERVICE_NAMESPACE]: 'my-namespace',
  }),
});

// Configure TLS with private internal CA
const traceExporter = new OTLPTraceExporter({
  url: 'https://core-gateway-internal.envoy-gateway-system.svc.cluster.local:4317/v1/traces',
  headers: {
    'x-internal-ca-trust': 'true',
  },
  credentials: {
    // Use Node's built-in TLS with private internal CA
    credentials: require('tls').createCredentials({
      ca: fs.readFileSync('/etc/otel-certs/ca.crt'),
    }),
  },
});

provider.addTracerProvider({
  exporter: traceExporter,
});

provider.register();

// Start tracing
const tracer = provider.getTracer('my-app');

function doSomething() {
  const span = tracer.startSpan('doSomething');
  span.setAttribute('operation', 'example');
  span.end();
}

doSomething();
```

### Example 3: Kubernetes Service with Internal TLS

```yaml
# Service exposing OTEL traces with TLS
apiVersion: v1
kind: Service
metadata:
  name: core-gateway-traces
  namespace: envoy-gateway-system
spec:
  selector:
    app.kubernetes.io/name: core-gateway
  ports:
    - name: otlp-grpc
      port: 4317
      targetPort: 4317
      protocol: TCP
    - name: otlp-http
      port: 4318
      targetPort: 4318
      protocol: TCP
  sessionAffinity: None
```

### Example 4: Complete TLS-Enabled OTEL Collector

```yaml
# charts/core-gateway/templates/otel-tls.yaml
apiVersion: opentelemetry.io/v1beta1
kind: OpenTelemetryCollector
metadata:
  name: {{ include "core-gateway.fullname" . }}-traces-tls
  namespace: {{ .Release.Namespace }}
spec:
  mode: deployment
  config:
    receivers:
      otlp:
        protocols:
          grpc:
            endpoint: 0.0.0.0:4317
            tls:
              insecure: false
              ca_file: /etc/otel-certs/ca.crt
              cert_file: /etc/otel-certs/server.crt
              key_file: /etc/otel-certs/server.key
          http:
            endpoint: 0.0.0.0:4318
            tls:
              insecure: false
              ca_file: /etc/otel-certs/ca.crt
              cert_file: /etc/otel-certs/server.crt
              key_file: /etc/otel-certs/server.key

    processors:
      memory_limiter:
        check_interval: 1s
        limit_mib: 400
        spike_limit_mib: 100
      batch:
        send_batch_size: 512
        timeout: 5s

    exporters:
      otlp/alloy:
        endpoint: "alloy.observability.svc.cluster.local:4317"
        tls:
          insecure: false
          ca_file: /etc/otel-certs/ca.crt
          cert_file: /etc/otel-certs/client.crt
          key_file: /etc/otel-certs/client.key

    service:
      pipelines:
        traces:
          receivers:
            - otlp
          processors:
            - memory_limiter
            - batch
          exporters:
            - otlp/alloy
```

### Example 5: Alloy with Mutual TLS

```yaml
# Alloy with mutual TLS (client cert required)
receivers:
  otlp:
    protocols:
      grpc:
        endpoint: 0.0.0.0:4317
        tls:
          insecure: false
          ca_file: /etc/otel-certs/ca.crt
          client_cert_file: /etc/otel-certs/client.crt
          client_key_file: /etc/otel-certs/client.key
      http:
        endpoint: 0.0.0.0:4318
        tls:
          insecure: false
          ca_file: /etc/otel-certs/ca.crt
          client_cert_file: /etc/otel-certs/client.crt
          client_key_file: /etc/otel-certs/client.key

exporters:
  tempo:
    endpoint: tempo:3200
    tls:
      insecure: false
      ca_file: /etc/otel-certs/ca.crt

service:
  pipelines:
    traces:
      receivers:
        - otlp
      processors:
        - memory_limiter
        - batch
      exporters:
        - tempo
```

---

## Troubleshooting & Debugging

### 1. Verify Private Internal CA Setup

```bash
# Check the Root CA ClusterIssuer
kubectl get clusterissuer self-signed-ca -o yaml

# Check the internal CA trust bundle
kubectl get secret internal-ca-trust-bundle -n librechat -o yaml

# Verify the CA certificate
kubectl get secret internal-ca-trust-bundle -n librechat -o jsonpath='{.data.ca\.crt}' | base64 -d | openssl x509 -noout -text

# Check service certificate
kubectl get secret core-gateway-internal-tls -n envoy-gateway-system -o jsonpath='{.data.tls\.crt}' | base64 -d | openssl x509 -text -noout
```

### 2. Test TLS Connection with Private Internal CA

```bash
# Test gRPC connection with private internal CA
kubectl run -it --rm debug --image=curlimages/curl --restart=Never -- \
  curl --cacert /etc/otel-certs/ca.crt \
       --cert /etc/otel-certs/client.crt \
       --key /etc/otel-certs/client.key \
       -X POST \
       -H "Content-Type: application/x-protobuf" \
       --data-binary @span.proto \
       https://alloy.observability.svc.cluster.local:4317/v1/traces

# Test HTTP connection with private internal CA
kubectl run -it --rm debug --image=curlimages/curl --restart=Never -- \
  curl --cacert /etc/otel-certs/ca.crt \
       --cert /etc/otel-certs/client.crt \
       --key /etc/otel-certs/client.key \
       -X POST \
       -H "Content-Type: application/x-protobuf" \
       --data-binary @span.proto \
       https://alloy.observability.svc.cluster.local:4318/v1/traces
```

### 3. Check OTEL Collector Logs

```bash
# Check OpenTelemetry Collector logs
kubectl logs -n envoy-gateway-system -l app.kubernetes.io/name=core-gateway-traces --tail=100

# Check Alloy logs
kubectl logs -n observability -l app.kubernetes.io/name=alloy --tail=100

# Check Tempo logs
kubectl logs -n observability -l app.kubernetes.io/name=tempo --tail=100
```

### 4. Verify Trace Data Flow

```bash
# Check if traces are reaching Alloy
kubectl port-forward -n observability svc/alloy 12345:12345 &
curl -s localhost:12345/api/v0/web/components | \
  python3 -c 'import sys,json;[print(c["localID"],c["health"]["state"]) for c in json.load(sys.stdin) if c["health"]["state"]!="healthy"]'

# Check Tempo for traces
kubectl port-forward -n observability svc/tempo 3200:3200 &
# Navigate to http://localhost:3200 in your browser
```

### 5. Common Issues and Solutions

#### Issue: "certificate signed by unknown authority"

**Solution:** Ensure the client trusts the private internal CA:

```yaml
# Add the CA certificate to the client
volumeMounts:
  - name: internal-ca
    mountPath: /etc/otel-certs
    readOnly: true

volumes:
  - name: internal-ca
    secret:
      secretName: internal-ca-trust-bundle
```

#### Issue: "connection refused"

**Solution:** Verify the service endpoint is correct:

```bash
# Check if the service exists
kubectl get svc alloy -n observability

# Check if the endpoint is ready
kubectl get endpoints alloy -n observability

# Test connectivity
kubectl run -it --rm debug --image=curlimages/curl --restart=Never -- \
  curl -v https://alloy.observability.svc.cluster.local:4317
```

#### Issue: "TLS handshake failed"

**Solution:** Verify certificate configuration:

```bash
# Check certificate validity
kubectl get secret core-gateway-internal-tls -n envoy-gateway-system -o jsonpath='{.data.tls\.crt}' | base64 -d | openssl x509 -noout -dates

# Check CA trust
kubectl get secret internal-ca-trust-bundle -n librechat -o jsonpath='{.data.ca\.crt}' | base64 -d | openssl x509 -noout -dates
```

#### Issue: "certificate does not match hostname"

**Solution:** Verify the certificate SAN matches the hostname:

```bash
# Check the certificate SAN
kubectl get secret core-gateway-internal-tls -n envoy-gateway-system -o jsonpath='{.data.tls\.crt}' | base64 -d | openssl x509 -noout -subject -issuer -dates
```

---

## Security Best Practices

### 1. Private Internal CA Management

```yaml
# Keep the Root CA private and secure
apiVersion: v1
kind: Secret
metadata:
  name: root-ca-key
  namespace: cert-manager
type: Opaque
data:
  # Store the Root CA private key securely
  key: <base64-encoded-root-ca-key>
```

**Best Practices:**
- Store Root CA private keys in Kubernetes Secrets with proper RBAC
- Use external secret stores (AWS Secrets Manager, HashiCorp Vault) for production
- Never commit private keys to version control
- Rotate intermediate CA certificates regularly

### 2. Certificate Rotation

```bash
# Generate new intermediate CA
openssl genrsa -out intermediate-ca.key 4096
openssl req -new -x509 -days 365 -key intermediate-ca.key \
  -out intermediate-ca.crt -CA root-ca.crt -CAkey root-ca.key \
  -CAcreateserial -sha256

# Update the ClusterIssuer
kubectl apply -f - <<EOF
apiVersion: cert-manager.io/v1
kind: ClusterIssuer
metadata:
  name: self-signed-ca
spec:
  selfSigned: {}
EOF
```

### 3. Network Security

```yaml
# Example CiliumNetworkPolicy for OTEL traffic
apiVersion: cilium.io/v2
kind: CiliumNetworkPolicy
metadata:
  name: allow-otel-to-alloy
  namespace: envoy-gateway-system
spec:
  endpointSelector:
    matchLabels:
      app.kubernetes.io/name: core-gateway-traces
  egress:
    - toEndpoints:
        - matchLabels:
            app.kubernetes.io/name: alloy
      toPorts:
        - ports:
            - port: "4317"
              protocol: TCP
      rules:
        http:
          - method: POST
            path: "/v1/traces"
```

### 4. Monitoring Certificate Expiration

```yaml
# Prometheus alert for expiring certificates
groups:
  - name: cert_expiry
    rules:
      - alert: CertificateExpiringSoon
        expr: |
          (time() - cert_expiry_timestamp_seconds) < 2592000
        for: 5m
        labels:
          severity: warning
        annotations:
          summary: "Certificate expiring in less than 30 days"
          description: "Certificate {{ $labels.secret }} will expire in {{ $value }} seconds"
```

### 5. Mutual TLS (mTLS) for High Security

```yaml
# Enable mutual TLS between OTEL components
receivers:
  otlp:
    protocols:
      grpc:
        endpoint: 0.0.0.0:4317
        tls:
          insecure: false
          ca_file: /etc/otel-certs/ca.crt
          client_cert_file: /etc/otel-certs/client.crt
          client_key_file: /etc/otel-certs/client.key
          verify_client_cert: true
```

### 6. Certificate Validation

```yaml
# Validate certificates on the server side
receivers:
  otlp:
    protocols:
      grpc:
        endpoint: 0.0.0.0:4317
        tls:
          insecure: false
          ca_file: /etc/otel-certs/ca.crt
          verify_client_cert: true
          client_ca_file: /etc/otel-certs/ca.crt
```

---

## Summary

### Key Points

1. **Private Internal CA** is a self-signed CA for internal services
2. **No external dependencies** - works offline and in air-gapped environments
3. **Never expires** - self-signed certificates don't need renewal
4. **Secure internal communication** - encrypts OTEL traces between components
5. **Fine-grained control** - control trust and certificate issuance

### Benefits for OTEL Traces

- ✅ **Secure** - Encrypts trace data in transit
- ✅ **Simple** - No external CA needed
- ✅ **Reliable** - No certificate expiration issues
- ✅ **Controlled** - Full control over trust and certificates

### Best Practices

- Keep Root CA private and secure
- Use external secret stores for production
- Implement certificate rotation
- Use mutual TLS for high-security environments
- Monitor certificate expiration
- Restrict network access to internal services

### Next Steps

1. Set up private internal CA in your cluster
2. Configure OTEL components with TLS
3. Implement certificate rotation automation
4. Set up monitoring for certificate expiration
5. Test end-to-end trace flow with TLS enabled

---

## References

- [cert-manager Documentation](https://cert-manager.io/docs/)
- [OpenTelemetry Collector Documentation](https://opentelemetry.io/docs/collector/)
- [Grafana Alloy Documentation](https://grafana.com/docs/alloy/)
- [Tempo Documentation](https://grafana.com/docs/tempo/latest/)
- [TLS Best Practices](https://tools.ietf.org/html/rfc5280)
- [Private CA Best Practices](https://csrc.nist.gov/publications/detail/sp/800-57-part-1/rev-5/final)