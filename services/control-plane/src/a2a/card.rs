//! The A2A agent card (RFC-0006 Phase 1): the public discovery document served at
//! `/.well-known/agent-card.json`.
//!
//! Phase 1 advertises exactly one skill — `review` — over two transports (JSON-RPC preferred, then
//! REST/HTTP+JSON), gated by an OIDC security scheme pointing at our Keycloak realm (ADR-0014).
//! Streaming and push notifications are advertised as unsupported (later phases). The wire shapes
//! are the v1.0.1 ones the #302 probe pinned (PascalCase methods, SCREAMING_SNAKE states,
//! camelCase card, ordered `supportedInterfaces[]`, `openIdConnectSecurityScheme`).

use std::collections::HashMap;

use a2a::{
    AgentCapabilities, AgentCard, AgentInterface, AgentSkill, OpenIdConnectSecurityScheme,
    SecurityScheme, TRANSPORT_PROTOCOL_HTTP_JSON, TRANSPORT_PROTOCOL_JSONRPC,
};

/// The security-scheme name referenced by the card. Callers authenticate with a Keycloak
/// client-credentials access token (service accounts; no anonymous access).
const OIDC_SCHEME_NAME: &str = "keycloak-oidc";

/// The `review` skill description. A short prose intro, then an inline JSON-Schema (draft-07) that
/// documents every field of the request object — `AgentSkill` has no `inputSchema` field, so the
/// description is the discoverable place to publish the input contract. The schema mirrors
/// [`super::mapping::parse_review_request`] exactly (its authoritative parser); keep the two in
/// sync. Full calling guide: `docs/a2a-review-skill.md`.
const REVIEW_SKILL_DESCRIPTION: &str = r#"Request a deep review of a pull/merge request. The review runs the deep tier through the same pipeline as an `@mention` and posts to the PR; the A2A task additionally returns, on completion, a summary + structured findings + a review-context part (the submitted baseSha/headSha, the request-derived scope diff|whole-tree, and the posted-review permalink) to the caller.

Send the target as the JSON object below, carried in a `data` part of a `ROLE_USER` message (`message.parts[].data`). You MAY also add one or more natural-language `text` parts carrying the review instruction (e.g. "focus on the auth changes and the new migration"): the `text` sets **emphasis only — never the target or scope**. The target (`repo`/`pr`/`headSha`) and scope (`baseSha`) come solely from the `data` part; a `text` part cannot redirect the review to a different PR/repo or change diff-vs-whole-tree. When `text` is present it becomes the review's prompt, winning over any `data.prompt` (a lower-priority hint); the text parts are concatenated in message order, newline-joined, and trimmed. A `text`-only message (no `data` part) is rejected with guidance naming the required precise fields, since this server holds no forge credentials to resolve a target from prose. Input schema (JSON Schema draft-07):

```json
{
  "$schema": "http://json-schema.org/draft-07/schema#",
  "title": "A2A review skill request",
  "type": "object",
  "required": ["repo", "pr", "headSha"],
  "properties": {
    "skill":   { "type": "string", "enum": ["review"], "default": "review", "description": "OPTIONAL skill selector. Only \"review\" exists in this phase; any other value is rejected (UNSUPPORTED_OPERATION). Defaults to \"review\" when omitted." },
    "forge":   { "type": "string", "enum": ["github", "gitlab"], "default": "github", "description": "OPTIONAL source forge, one of \"github\" | \"gitlab\". Selects which platform the repo/PR is resolved against. Defaults to \"github\"." },
    "repo":    { "type": "string", "pattern": "^[^/]+/[^/]+$", "description": "REQUIRED repository slug as \"owner/name\" (exactly one slash; surrounding whitespace is trimmed). Must be an already-approved/provisioned repo on the selected forge, else the task is REJECTED." },
    "pr":      { "type": ["integer", "string"], "description": "REQUIRED PR (GitHub) / MR (GitLab) number. Integer > 0, given as a JSON integer or a numeric string, e.g. 164 or \"164\". Identifies which change set to review." },
    "headSha": { "type": "string", "minLength": 1, "description": "REQUIRED commit SHA to review — the exact PR/MR head. The repo is checked out at this commit and the review runs against it. This server holds NO forge credentials and cannot resolve a head itself, so an absent head is REJECTED (a null head would silently review the default branch). Also accepted under the snake_case key head_sha." },
    "baseSha": { "type": "string", "minLength": 1, "description": "OPTIONAL but STRONGLY RECOMMENDED base commit SHA — the PR/MR's base (target-branch) commit. SCOPING EFFECT: when present, the review is DIFF-SCOPED to just the PR's changes (the runner computes git diff merge-base(baseSha, headSha)..headSha — the same three-dot 'Files changed' set the forge shows). When ABSENT, no diff can be computed and the review FALLS BACK to the WHOLE WORKING TREE at headSha — a broader, unfocused audit of the entire repo snapshot, NOT the PR's delta. The role can't resolve the base itself (no forge credentials), so the caller must supply it to get a diff-scoped review. Also accepted under the snake_case key base_sha." },
    "prompt":  { "type": "string", "description": "OPTIONAL free-text focus prompt, recorded as the run's intent and shown to the agent (e.g. 'focus on the auth changes'). It steers emphasis but does NOT change scope (diff vs whole-tree is decided by baseSha) or target. A natural-language `text` part on the message, if present, is the preferred way to carry this and WINS over this field (which is then a lower-priority hint). Omitted with no text part → a generic deep-review intent." }
  }
}
```

Scoping (diff vs whole-tree): supply `baseSha` for a review scoped to the PR's changes; omit it and the review runs against the whole working tree at `headSha` instead — see the `baseSha` description above.

Wire note: enums are ProtoJSON SCREAMING_SNAKE — the message `role` is `ROLE_USER` and task states are `TASK_STATE_*`. Unknown keys are ignored (the parser also accepts the snake_case SHA aliases above). See `docs/a2a-review-skill.md` for a full curl walkthrough, the response shape, and the GetTask polling loop."#;

/// Build the agent card the `a2a` role publishes.
///
/// * `base_url` — the externally reachable base URL of this role's A2A endpoints (both transports
///   are served under it; the Ingress host is an ai-helm concern, out of scope for this PR).
/// * `oidc_discovery_url` — the OIDC discovery document URL for the realm callers authenticate
///   against (`{issuer}/.well-known/openid-configuration`).
pub fn build_agent_card(base_url: &str, oidc_discovery_url: &str) -> AgentCard {
    let mut security_schemes = HashMap::new();
    security_schemes.insert(
        OIDC_SCHEME_NAME.to_string(),
        SecurityScheme::OpenIdConnect(OpenIdConnectSecurityScheme {
            open_id_connect_url: oidc_discovery_url.to_string(),
            description: Some(
                "Keycloak OIDC; callers are client-credentials service accounts (ADR-0014). \
                 The `review` skill additionally requires the `a2a:review` permission (ADR-0023)."
                    .to_string(),
            ),
        }),
    );

    AgentCard {
        name: "Lightbridge Code Intelligence".to_string(),
        description: "A2A surface over Lightbridge's deep code-review agent. Request a deep review \
                      of a pull/merge request and poll the resulting task for a summary + findings."
            .to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        // Ordered: the first entry is the preferred transport (spec §5.6.4). Both are served by this
        // role; JSON-RPC first because Bedrock AgentCore and most current peers speak it.
        supported_interfaces: vec![
            AgentInterface::new(base_url.to_string(), TRANSPORT_PROTOCOL_JSONRPC),
            AgentInterface::new(base_url.to_string(), TRANSPORT_PROTOCOL_HTTP_JSON),
        ],
        // Phase 2 (ADR-0077): streaming is live (SubscribeToTask + the streaming leg of SendMessage).
        // Phase 3 (ADR-0079): push notifications are live — a caller registers a webhook via
        // `CreateTaskPushNotificationConfig` and the `notifier` role POSTs task updates to it. No
        // extended card.
        capabilities: AgentCapabilities {
            streaming: Some(true),
            push_notifications: Some(true),
            extensions: None,
            extended_agent_card: None,
        },
        // `application/json` for the structured target `data` part; `text/plain` for the optional
        // natural-language instruction part (ADR-0078 — text sets emphasis only, never target/scope).
        default_input_modes: vec![
            "application/json".to_string(),
            "text/plain".to_string(),
        ],
        default_output_modes: vec!["text/plain".to_string(), "application/json".to_string()],
        skills: vec![AgentSkill {
            id: "review".to_string(),
            name: "Deep PR review".to_string(),
            description: REVIEW_SKILL_DESCRIPTION.to_string(),
            tags: vec![
                "code-review".to_string(),
                "ci".to_string(),
                "static-analysis".to_string(),
            ],
            examples: Some(vec![
                // Minimal: the four required fields (skill defaults to `review`, forge to `github`).
                // `pr` is shown as a numeric string — the accepted form regardless of how a peer's
                // ProtoJSON codec renders numbers; a JSON integer works too.
                r#"{"skill":"review","repo":"acme/api","pr":"164","headSha":"9f2a1c4e8b7d6053a1f4c2e9b8d70a5c3e1f2b6d"}"#
                    .to_string(),
                // Full: every optional field (forge, baseSha, prompt) alongside the required ones.
                r#"{"skill":"review","forge":"github","repo":"acme/api","pr":"164","headSha":"9f2a1c4e8b7d6053a1f4c2e9b8d70a5c3e1f2b6d","baseSha":"1b0dd7a4c9e2f6538a0c4b1e9d7f2a5c3e8b6d04","prompt":"Focus on the auth changes and the new migration."}"#
                    .to_string(),
                // GitLab merge request, minimal.
                r#"{"skill":"review","forge":"gitlab","repo":"acme/platform","pr":"57","headSha":"3e8b6d041b0dd7a4c9e2f6538a0c4b1e9d7f2a5c"}"#
                    .to_string(),
                // ADR-0078: a natural-language instruction (`text` part) alongside the structured
                // target (`data` part) — the full `message.parts` shape. The text sets emphasis only;
                // the target/scope still come solely from the data part. Here it also wins over any
                // `data.prompt`.
                r#"{"parts":[{"text":"Focus on the auth changes and the new migration; check the token TTL."},{"data":{"skill":"review","repo":"acme/api","pr":"164","headSha":"9f2a1c4e8b7d6053a1f4c2e9b8d70a5c3e1f2b6d","baseSha":"1b0dd7a4c9e2f6538a0c4b1e9d7f2a5c3e8b6d04"}}]}"#
                    .to_string(),
            ]),
            input_modes: Some(vec![
                "application/json".to_string(),
                "text/plain".to_string(),
            ]),
            output_modes: Some(vec![
                "text/plain".to_string(),
                "application/json".to_string(),
            ]),
            // Per-skill security: the `a2a:review` permission is enforced at submission (ADR-0023).
            security_requirements: Some(vec![HashMap::from([(
                OIDC_SCHEME_NAME.to_string(),
                vec!["a2a:review".to_string()],
            )])]),
        }],
        provider: None,
        documentation_url: None,
        icon_url: None,
        security_schemes: Some(security_schemes),
        // Card-level requirement: every call authenticates with the OIDC scheme.
        security_requirements: Some(vec![HashMap::from([(
            OIDC_SCHEME_NAME.to_string(),
            Vec::new(),
        )])]),
        signatures: None,
    }
}

/// Derive the OIDC discovery URL from the issuer (Keycloak convention), or a clearly-invalid
/// placeholder when the issuer is unset (the role fails readiness in that case anyway).
pub fn oidc_discovery_url(issuer: Option<&str>) -> String {
    match issuer {
        Some(iss) => format!(
            "{}/.well-known/openid-configuration",
            iss.trim_end_matches('/')
        ),
        None => "https://keycloak.invalid/realms/lightbridge/.well-known/openid-configuration"
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "https://a2a.lightbridge.example/a2a";
    const OIDC: &str = "https://kc.example/realms/lightbridge/.well-known/openid-configuration";

    #[test]
    fn card_conforms_to_v1_0_1_wire_shapes() {
        let card = serde_json::to_value(build_agent_card(BASE, OIDC)).expect("serializes");

        // camelCase field names.
        assert!(card.get("supportedInterfaces").is_some());
        assert!(card.get("defaultInputModes").is_some());
        assert!(card.get("securitySchemes").is_some());

        // Ordered supportedInterfaces[]: JSON-RPC preferred, REST second, each with a protocolVersion.
        let ifaces = card["supportedInterfaces"].as_array().unwrap();
        assert_eq!(ifaces.len(), 2);
        assert_eq!(ifaces[0]["protocolBinding"], "JSONRPC");
        assert_eq!(ifaces[1]["protocolBinding"], "HTTP+JSON");
        for iface in ifaces {
            assert!(iface["protocolVersion"].as_str().is_some());
        }

        // Phase 2/3 capabilities: streaming on (ADR-0077); push on (ADR-0079, delivery now exists).
        assert_eq!(card["capabilities"]["streaming"], true);
        assert_eq!(card["capabilities"]["pushNotifications"], true);

        // Exactly the `review` skill, gated by `a2a:review`.
        let skills = card["skills"].as_array().unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0]["id"], "review");
        assert_eq!(
            skills[0]["securityRequirements"][0]["keycloak-oidc"][0],
            "a2a:review"
        );

        // Self-documenting: multiple examples of usage, each a parseable JSON object; a data-object
        // example carries the required target fields, and the ADR-0078 multipart example carries the
        // target in a `data` part next to a natural-language `text` instruction part.
        let examples = skills[0]["examples"].as_array().unwrap();
        assert!(
            examples.len() >= 2,
            "review skill should advertise multiple usage examples"
        );
        let mut saw_text_part = false;
        for ex in examples {
            let obj: serde_json::Value =
                serde_json::from_str(ex.as_str().unwrap()).expect("example is valid JSON");
            // Two shapes: a bare `data` object, or a full `{ "parts": [...] }` message.
            let data = if let Some(parts) = obj.get("parts").and_then(|p| p.as_array()) {
                // A multipart example (ADR-0078) must carry a text instruction part…
                assert!(
                    parts.iter().any(|p| p.get("text").is_some()),
                    "multipart example carries a natural-language text part"
                );
                saw_text_part = true;
                // …and the target in a data part.
                parts
                    .iter()
                    .find_map(|p| p.get("data"))
                    .expect("multipart example carries a data target part")
                    .clone()
            } else {
                obj
            };
            assert_eq!(data["skill"], "review");
            assert!(data.get("repo").is_some() && data.get("pr").is_some());
            assert!(
                data["headSha"].as_str().is_some(),
                "every example's target carries the required headSha"
            );
        }
        assert!(
            saw_text_part,
            "the skill advertises the ADR-0078 text+data form"
        );

        // Input modes advertise both the structured target and the natural-language instruction.
        let input_modes = skills[0]["inputModes"].as_array().unwrap();
        assert!(input_modes.iter().any(|m| m == "application/json"));
        assert!(
            input_modes.iter().any(|m| m == "text/plain"),
            "text/plain is an accepted input mode for the NL instruction (ADR-0078)"
        );
        let default_modes = card["defaultInputModes"].as_array().unwrap();
        assert!(default_modes.iter().any(|m| m == "text/plain"));

        // The description states plainly that text sets emphasis only — never target or scope.
        let desc = skills[0]["description"].as_str().unwrap();
        assert!(
            desc.contains("emphasis only") && desc.contains("never the target or scope"),
            "description states text sets emphasis only, not target/scope"
        );
        let description = skills[0]["description"].as_str().unwrap();
        assert!(
            description.contains("json-schema.org/draft-07"),
            "description embeds an inline JSON-Schema"
        );
        for field in [
            "repo", "pr", "headSha", "baseSha", "forge", "prompt", "skill",
        ] {
            assert!(
                description.contains(field),
                "schema in description documents `{field}`"
            );
        }
        assert!(
            description.contains("ROLE_USER") && description.contains("data"),
            "description states the ROLE_USER + data-part wire form"
        );

        // OIDC security scheme under the field-presence variant key.
        let oidc = &card["securitySchemes"]["keycloak-oidc"];
        assert_eq!(
            oidc["openIdConnectSecurityScheme"]["openIdConnectUrl"], OIDC,
            "expected openIdConnectSecurityScheme variant with our discovery url"
        );
    }

    #[test]
    fn discovery_url_follows_keycloak_convention_and_trims_slash() {
        assert_eq!(
            oidc_discovery_url(Some("https://kc.example/realms/lightbridge/")),
            "https://kc.example/realms/lightbridge/.well-known/openid-configuration"
        );
        assert!(oidc_discovery_url(None).contains(".well-known/openid-configuration"));
    }
}
