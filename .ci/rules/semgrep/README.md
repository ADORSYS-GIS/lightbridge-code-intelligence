# Semgrep Rule Pack

This directory contains repository-specific and community SAST rules for Semgrep.

## Rules Organization

- **security/** — Security-focused rules (injection, authentication, cryptography)
- **reliability/** — Code reliability (error handling, null checks)
- **best-practices/** — Code style and maintainability
- **owasp/** — OWASP Top 10 rules (adopted from Semgrep registry)

## Adding New Rules

1. Create a new `.yaml` or `.json` rule file in the appropriate subdirectory.
2. Test locally: `semgrep --config=.ci/rules/semgrep path/to/code`
3. Commit the rule and document it in this README.
4. Rules are automatically picked up by the CI workflow.

## Rule Format

Semgrep rules follow YAML format with required fields:

```yaml
rules:
  - id: custom-rule-id
    pattern-either:
      - pattern: |
          eval(...)
      - pattern: |
          exec(...)
    message: "Unsafe use of eval() detected"
    languages: [python]
    severity: ERROR
    metadata:
      cwe: "CWE-95: Improper Neutralization of Directives in Dynamically Evaluated Code ('Eval Injection')"
      owasp: "A03:2021 – Injection"
      references:
        - "https://example.com"
```

## Testing Rules Locally

```bash
# Test a single rule against a file.
semgrep -c .ci/rules/semgrep/security/example.yaml path/to/file.ts

# Test all rules in the directory.
semgrep --config=.ci/rules/semgrep path/to/code

# Generate SARIF output.
semgrep --config=.ci/rules/semgrep --sarif --output=test.sarif path/to/code
```

## Maintenance

- Review and update rules quarterly or when new vulnerability patterns emerge.
- Remove rules that are no longer applicable to the codebase.
- Document the reason for adding/removing rules in commit messages.
- Keep rule IDs stable; avoid renaming existing rules (affects baseline compatibility).

## References

- [Semgrep Documentation](https://semgrep.dev/docs/)
- [Rule Writing Guide](https://semgrep.dev/docs/writing-rules/rule-basics/)
- [Semgrep Registry (community rules)](https://semgrep.dev/r)
- [CWE / OWASP mappings](https://semgrep.dev/docs/writing-rules/metadata/)
