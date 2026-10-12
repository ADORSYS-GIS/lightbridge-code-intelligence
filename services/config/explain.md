# Lightbridge Config — `services/config/`

## What is it?

A **shared, minimal-dependency configuration loader crate** (`lightbridge-config`) used by both
the control plane and the agent runner. It loads JSON config files (mounted from Helm ConfigMaps)
and supports **`{env:VAR:-default}` substitution** within string values — so secrets and
per-environment values stay in environment variables while the declarative config structure
lives in the ConfigMap.

---

## Why JSON, not YAML?

- **Zero additional dependencies** — serde + serde_json are already in the dependency tree
- Templates are **separate mounted files** (system prompts, etc.), so the config file itself
  is only scalars and paths
- JSON is simpler to generate from Helm templates

---

## Core API

```rust
// Load a typed config from a JSON file (with env substitution)
let cfg: MyConfig = lightbridge_config::load(path)?;

// Load a template file with env substitution
let prompt = lightbridge_config::load_template(path)?;

// Substitute env vars in any string
let resolved = lightbridge_config::substitute_env("{env:HOSTNAME} is online");

// Substitute through a whole parsed JSON tree
lightbridge_config::substitute_value(&mut value, &lookup_fn);
```

---

## `{env:VAR:-default}` substitution

The distinctive feature: **every string value** in a loaded config file is scanned for
`{env:NAME}` or `{env:NAME:-default}` references, which are replaced with the environment
variable's value or the inline default (or empty string if neither exists).

**Examples:**
```jsonc
{
  // From env var, fail in config if unset (required field):
  "api_key": "{env:LLM_API_KEY}",

  // From env var with a default:
  "model": "{env:LLM_MODEL:-deepseek-coder}",

  // URLs work too (the `:-` separator splits on the first occurrence):
  "base_url": "{env:LLM_BASE_URL:-https://gw:443/v1}",

  // Various types — all strings at the JSON level:
  "timeout": "{env:LLM_TIMEOUT:-180}",
  "stream": "{env:LLM_STREAM:-true}"
}
```

The substitution happens **before** typed deserialization, so a field expecting `u64`, `f64`,
or `bool` will receive the substituted string — the crate provides custom `serde` deserializers
to handle this:

```rust
#[derive(Deserialize)]
struct Config {
    #[serde(default, deserialize_with = "lightbridge_config::de::opt_u64")]
    timeout: Option<u64>,

    #[serde(default, deserialize_with = "lightbridge_config::de::opt_f64")]
    temperature: Option<f64>,

    #[serde(default, deserialize_with = "lightbridge_config::de::opt_bool")]
    stream: Option<bool>,
}
```

---

## Design principles

1. **Best-effort at the call site** — both the control plane and agent runner treat a missing
   config path as "use built-in defaults / legacy env vars", so prod keeps running before the
   ConfigMap is mounted.

2. **Substitution is recursive** — `substitute_value` walks the parsed JSON tree (objects,
   arrays, strings) and replaces in-place before deserialization. A value like
   `"{env:LLM_API_KEY}"` resolves regardless of where it sits in the schema hierarchy.

3. **No subprocess or external tool** — pure Rust string manipulation, no shelling out to `envsubst`.

4. **Testable** — `substitute_with` accepts an injectable `lookup: impl Fn(&str) -> Option<String>`,
   so tests don't touch the process environment.

---

## Files

| File | Contents |
|------|----------|
| `Cargo.toml` | Package manifest: `lightbridge-config`, only depends on serde + serde_json + anyhow + tracing |
| `src/lib.rs` | `substitute_with`, `substitute_env`, `substitute_value`, `load`, `load_with`, `load_template`, `de` module (opt_u64, opt_f64, opt_bool, opt_i64, opt_usize) |

---

## Usage in the project

| Consumer | How it uses `lightbridge-config` |
|----------|--------------------------------|
| `control-plane` | Loads `control-plane.json` for `review` section (PR feedback config) + `embeddings` section (dimension safety) |
| `agent-runner` | Loads `agent.json` for `embeddings`, `review` (both tiers), and `sast` configuration blocks |
