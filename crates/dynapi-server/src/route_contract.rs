//! Source-derived origin metadata for tooling; deployment admission is external.
use serde_json::{Value, json};

/// The manifest contains origin operation intent, never deployment admission decisions.
pub fn document() -> Value {
    let routes: Vec<_> = crate::ROUTES
        .iter()
        .map(|route| {
            let intent = match route.template {
                "/" => "redirect",
                "/robots.txt" => "static",
                "/healthz" => "health",
                "/openapi.json" | "/docs" | "/docs/oauth2-redirect" | "/redoc" => "documentation",
                "/v1/data-products" | "/v1/data-products/{dataProductId}" => "catalog",
                "/v1/forecasts"
                | "/v1/analyses"
                | "/v1/forecasts/runs"
                | "/v1/data/{dataProductId}/{snapshotId}/points/{gridIndices}" => "data",
                other => panic!("route needs an operation-intent annotation: {other}"),
            };
            json!({
                "template": route.template,
                "intent": intent,
                "methods": route.methods,
                "directRedirectStatus": (route.template == "/").then_some(301),
                "head": {"registered": route.methods.contains(&"HEAD"), "body": "suppressed"},
                "plainOptionsStatus": if route.cors {204} else {405},
                "slashAlternativeRedirect": route.template != "/",
            })
        })
        .collect();
    json!({
        "schemaVersion": 1,
        "scope": "origin",
        "pathBehavior": {
            "percentDecoding": "once_utf8_lossy",
            "parameters": "one_nonempty_decoded_segment",
            "staticTerminalLf": "one_tolerated",
            "unmatchedStatus": 404,
            "slashRedirect": {
                "status": 307,
                "removeTrailingSlashes": "all",
                "preserveQuery": true,
                "scheme": "configured_public_scheme",
                "authority": "validated_host_or_socket",
            },
        },
        "methodBehavior": {
            "unsupportedStatus": 405,
            "allow": "registered_methods",
            "headBody": "suppressed_including_errors",
        },
        "preflight": {
            "method": "OPTIONS",
            "requiresHeaders": ["Origin", "Access-Control-Request-Method"],
            "scope": "all_paths",
            "successStatus": 200,
            "rejectionStatus": 400,
            "beforeOriginAuthentication": true,
            "beforeRouting": true,
        },
        "routes": routes,
    })
}

/// Canonical exporter output, including its final newline.
pub fn render() -> String {
    let mut document = document();
    document.sort_all_objects();
    format!("{}\n", serde_json::to_string_pretty(&document).unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn committed_manifest_matches_router_inventory() {
        assert_eq!(
            render(),
            include_str!("../../../docs/route-contract.v1.json"),
            "regenerate with cargo run -p dynapi-server --features harness --example route-contract"
        );
        let document = document();
        let mut templates = BTreeSet::new();
        let mut cells = BTreeSet::new();
        for route in document["routes"].as_array().unwrap() {
            let template = route["template"].as_str().unwrap();
            assert!(templates.insert(template));
            assert!(!route["methods"].as_array().unwrap().is_empty());
            for method in route["methods"].as_array().unwrap() {
                let method = method.as_str().unwrap();
                assert!(method.bytes().all(|byte| byte.is_ascii_uppercase()));
                assert!(cells.insert((template, method)));
            }
        }
    }
}
