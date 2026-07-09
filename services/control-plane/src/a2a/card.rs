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
        // Phase 1 is polling-only: no streaming, no push notifications, no extended card.
        capabilities: AgentCapabilities {
            streaming: Some(false),
            push_notifications: Some(false),
            extensions: None,
            extended_agent_card: None,
        },
        default_input_modes: vec!["application/json".to_string()],
        default_output_modes: vec!["text/plain".to_string(), "application/json".to_string()],
        skills: vec![AgentSkill {
            id: "review".to_string(),
            name: "Deep PR review".to_string(),
            description: "Request a deep review of a pull/merge request. Input is a `data` part \
                          with `repo` (owner/name) and `pr`; optional `forge` (default github), \
                          `prompt`, and `headSha`/`baseSha`. The review posts to the PR through the \
                          existing pipeline; the A2A task additionally returns the summary + findings."
                .to_string(),
            tags: vec![
                "code-review".to_string(),
                "ci".to_string(),
                "static-analysis".to_string(),
            ],
            examples: Some(vec![
                "{\"skill\":\"review\",\"repo\":\"acme/api\",\"pr\":128}".to_string()
            ]),
            input_modes: Some(vec!["application/json".to_string()]),
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

        // Phase 1 capabilities: no streaming, no push.
        assert_eq!(card["capabilities"]["streaming"], false);
        assert_eq!(card["capabilities"]["pushNotifications"], false);

        // Exactly the `review` skill, gated by `a2a:review`.
        let skills = card["skills"].as_array().unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0]["id"], "review");
        assert_eq!(
            skills[0]["securityRequirements"][0]["keycloak-oidc"][0],
            "a2a:review"
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
