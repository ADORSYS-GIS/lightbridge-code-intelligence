---
"lightbridge-code-intelligence": minor
---

feat(quality): SonarQube CE replacement with offline-capable scanning

Replaces SonarQube CE with modular offline-capable quality scanning:
- SAST: Semgrep (local rules)
- Dependencies: Trivy (pre-cached offline DBs)
- Secrets: Gitleaks
- Dockerfiles: Hadolint
- TypeScript/JS: Biome

PR: #523
