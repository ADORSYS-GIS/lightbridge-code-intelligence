# Deployment

Lightbridge Code Intelligence runs on the **home-remote** Kubernetes cluster, deployed by
ArgoCD. The Helm chart and the ArgoCD `Application` live in the
[`ADORSYS-GIS/ai-helm`](https://github.com/ADORSYS-GIS/ai-helm) GitOps repo
(`charts/lightbridge-code-intelligence`, Application `aii-lightbridge-code-intelligence`).

## Continuous deployment (image-updater + git write-back)

```
push to main ──▶ build-images.yml ──▶ ghcr.io/vymalo/lightbridge-{control-plane,web}:sha-<gitsha>
                                       (+ keyless cosign signature)
                                                │
                          argocd-image-updater  │ picks newest *signed* sha-<gitsha>,
                                                 ▼ verifies cosign identity, then git-commits
                                                   the tag into the PRIVATE values repo ──▶
                  adorsys-gis/ai-helm-values: environments/prod/values/
                                       lightbridge-code-intelligence.yaml  (main)
                                                │
                                ArgoCD $values  ▼ auto-syncs the new tag
                                       lightbridge-ci-{control-plane,web} pods roll
```

The production image tags are **no longer stored in this repo** — they moved to
[`adorsys-gis/ai-helm-values`](https://github.com/adorsys-gis/ai-helm-values)
(`environments/prod/values/lightbridge-code-intelligence.yaml`) on 2026-06-23. This repo's
`main` became PR-protected, which rejected argocd-image-updater's direct write-back push
(`GH006`); the values repo's `main` is unprotected, so the bot can commit again (same target
repo as `converse-ui`). ai-helm's Application reads that file as its ArgoCD `$values` source
(`targetRevision: main`).

- **Image signing** — `.github/workflows/build-images.yml` cosign-signs every pushed digest
  (keyless, GitHub OIDC). image-updater verifies the signature traces back to this workflow
  before promoting an image.

This repo's `deploy/` now holds only the **observability** dashboards/chart
(`deploy/observability/`), the **Keycloak realm** export (`deploy/keycloak/`), and the
[**configuration & environment variable reference**](config-reference.md) — a single documentation
page, not something ArgoCD reads. The chart itself tracks `ai-helm` `main`; only the image tags flow
continuously, now from `ai-helm-values`.
