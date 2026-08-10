use std::collections::BTreeSet;

use chrono::{Duration, Utc};
use octostore::models::{
    AcquireLockResponse, CreateSessionResponse, CreateWebhookRequest, LockEventType, LockWatchEvent,
};
use octostore::webhooks::WebhookStore;
use serde::Serialize;
use serde_yaml::Value;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

fn document() -> Value {
    serde_yaml::from_str(include_str!("../openapi.yaml")).expect("OpenAPI must parse")
}

fn required_fields(document: &Value, schema: &str) -> BTreeSet<String> {
    document["components"]["schemas"][schema]["required"]
        .as_sequence()
        .unwrap_or_else(|| panic!("{schema}.required must be a sequence"))
        .iter()
        .map(|field| {
            field
                .as_str()
                .expect("required field must be text")
                .to_string()
        })
        .collect()
}

fn serialized_fields(value: &impl Serialize) -> BTreeSet<String> {
    serde_json::to_value(value)
        .expect("implementation response must serialize")
        .as_object()
        .expect("implementation response must be an object")
        .keys()
        .cloned()
        .collect()
}

#[test]
fn representative_rust_responses_match_openapi_required_fields() {
    let document = document();
    let acquired = AcquireLockResponse::Acquired {
        lease_id: Uuid::new_v4(),
        fencing_token: 7,
        expires_at: Utc::now() + Duration::seconds(30),
        renew_after_ms: 15_000,
        metadata: None,
    };
    assert_eq!(
        serialized_fields(&acquired),
        required_fields(&document, "LockAcquired")
    );

    let session = CreateSessionResponse {
        session_id: Uuid::new_v4(),
        expires_at: Utc::now() + Duration::seconds(60),
        keepalive_interval_secs: 30,
    };
    assert_eq!(
        serialized_fields(&session),
        required_fields(&document, "CreateSessionResponse")
    );
}

#[test]
fn acl_validation_and_session_clamping_are_documented_exactly() {
    let document = document();
    let acl_responses = &document["paths"]["/locks/{name}/acl"]["put"]["responses"];
    assert!(acl_responses["400"].is_mapping());
    assert!(acl_responses["422"].is_mapping());

    let ttl = &document["paths"]["/sessions"]["post"]["requestBody"]["content"]["application/json"]
        ["schema"]["properties"]["ttl_seconds"];
    assert!(
        ttl["minimum"].is_null(),
        "clamped input must not claim a strict minimum"
    );
    assert!(
        ttl["maximum"].is_null(),
        "clamped input must not claim a strict maximum"
    );
    let description = ttl["description"]
        .as_str()
        .expect("session TTL needs a clamping description");
    assert!(description.contains("clamps"));
    assert!(description.contains("10–300"));
}

#[test]
fn public_holder_identifier_is_explicitly_non_actionable() {
    let document = document();
    for schema in ["LockHeld", "LockStatus"] {
        let description = document["components"]["schemas"][schema]["properties"]["holder_id"]
            ["description"]
            .as_str()
            .unwrap_or_else(|| panic!("{schema}.holder_id needs a safety description"));
        assert!(description.contains("pseudonym"));
        assert!(description.contains("not"));
    }
}

#[test]
fn election_watch_uses_one_rate_limit_contract_for_all_admission_bounds() {
    let document = document();
    let responses = &document["paths"]["/elections/{election_id}/watch"]["get"]["responses"];
    assert!(responses.get("409").is_none());
    assert_eq!(
        responses["429"]["$ref"].as_str(),
        Some("#/components/responses/PublicElectionRateLimited")
    );
}

#[test]
fn lock_watch_contract_matches_the_initial_snapshot_and_sse_wire_shape() {
    let document = document();
    let operation = &document["paths"]["/locks/{name}/watch"]["get"];
    let description = operation["description"]
        .as_str()
        .expect("lock watch needs an exact snapshot contract");
    for required in [
        "first frame",
        "current-state snapshot",
        "`acquired` when the lock is held",
        "`released` when it is vacant",
        "duplicate state hint",
        "GET /locks/{name}",
    ] {
        assert!(description.contains(required), "missing '{required}'");
    }

    let event_stream = &operation["responses"]["200"]["content"]["text/event-stream"];
    assert_eq!(event_stream["schema"]["type"].as_str(), Some("string"));
    let example = event_stream["example"]
        .as_str()
        .expect("lock SSE needs a wire-frame example");
    let payload = example
        .strip_prefix("data: ")
        .expect("SSE example must be a data frame");
    let payload: serde_json::Value = serde_json::from_str(payload).unwrap();
    assert_eq!(payload["event"], "acquired");
    assert_eq!(payload["lock_name"], "jobs/import");

    let acquired_snapshot = LockWatchEvent {
        event: LockEventType::Acquired,
        lock_name: "jobs/import".to_string(),
        fencing_token: Some(42),
        expires_at: Some(Utc::now() + Duration::seconds(60)),
        observed_at: Utc::now(),
    };
    assert_eq!(
        serialized_fields(&acquired_snapshot),
        payload
            .as_object()
            .expect("SSE example payload must be an object")
            .keys()
            .cloned()
            .collect()
    );

    let vacant_snapshot = LockWatchEvent {
        event: LockEventType::Released,
        lock_name: "jobs/import".to_string(),
        fencing_token: None,
        expires_at: None,
        observed_at: Utc::now(),
    };
    assert_eq!(
        serialized_fields(&vacant_snapshot),
        ["event", "lock_name", "observed_at"]
            .into_iter()
            .map(str::to_string)
            .collect()
    );
}

#[test]
fn election_mutation_errors_and_unsigned_delays_are_documented_exactly() {
    let document = document();
    for operation in ["renew", "resign"] {
        let path = format!("/elections/{{election_id}}/{operation}");
        assert_eq!(
            document["paths"][path.as_str()]["post"]["responses"]["404"]["$ref"].as_str(),
            Some("#/components/responses/LeaseNotCurrent")
        );
    }

    for (schema, field) in [
        ("ElectionStatus", "retry_after_ms"),
        ("ElectionLeaderResult", "renew_after_ms"),
        ("ElectionFollowerResult", "retry_after_ms"),
    ] {
        assert_eq!(
            document["components"]["schemas"][schema]["properties"][field]["minimum"].as_u64(),
            Some(0),
            "{schema}.{field} must preserve the unsigned Rust contract"
        );
    }
}

#[test]
fn oauth_handoff_is_single_use_and_never_documents_a_bearer_in_location() {
    let document = document();
    let callback = &document["paths"]["/auth/github/callback"]["get"];
    let state = callback["parameters"]
        .as_sequence()
        .unwrap()
        .iter()
        .find(|parameter| parameter["name"].as_str() == Some("state"))
        .expect("callback must document state");
    assert_eq!(state["required"].as_bool(), Some(true));
    let location = callback["responses"]["303"]["headers"]["Location"]["schema"]["example"]
        .as_str()
        .expect("callback redirect needs a safe example");
    assert!(location.contains("#exchange_code="));
    assert!(location.contains("&issuer=https%3A%2F%2Fapi.octostore.io"));
    assert!(!location.contains("token="));

    let exchange = &document["paths"]["/auth/github/exchange"]["post"];
    assert_eq!(
        exchange["responses"]["200"]["content"]["application/json"]["schema"]["$ref"].as_str(),
        Some("#/components/schemas/AuthToken")
    );
    assert_eq!(
        exchange["responses"]["200"]["headers"]["Cache-Control"]["schema"]["enum"][0].as_str(),
        Some("no-store")
    );
}

#[test]
fn oauth_callback_documents_the_implemented_upstream_failure_contract() {
    let document = document();
    let response = &document["paths"]["/auth/github/callback"]["get"]["responses"]["502"];
    assert_eq!(
        response["$ref"].as_str(),
        Some("#/components/responses/UpstreamUnavailable")
    );

    let implementation = octostore::error::AppError::UpstreamUnavailable {
        service: "GitHub OAuth token exchange",
    };
    let response = axum::response::IntoResponse::into_response(implementation);
    assert_eq!(response.status(), axum::http::StatusCode::BAD_GATEWAY);
}

#[test]
fn lock_name_contract_preserves_unicode_alphanumeric_components() {
    let document = document();
    let schema = &document["components"]["parameters"]["LockName"]["schema"];
    assert!(
        schema["pattern"].is_null(),
        "an ASCII-only OpenAPI regex would reject names accepted by Rust"
    );
    assert!(
        schema["maxLength"].is_null(),
        "JSON Schema character length cannot express Rust's UTF-8 byte ceiling"
    );
    let description = document["components"]["parameters"]["LockName"]["description"]
        .as_str()
        .expect("lock names need an exact character contract");
    assert!(description.contains("Unicode alphanumeric"));
    assert!(description.contains("64 UTF-8 bytes"));
    assert!(description.contains("256 UTF-8 bytes total"));

    assert!(octostore::models::validate_lock_name("équipe/分队-1").is_ok());
    assert!(octostore::models::validate_lock_name("équipe/has space").is_err());
}

#[test]
fn admin_and_webhook_operations_publish_finite_response_contracts() {
    let document = document();
    for (path, schema) in [
        ("/admin/status", "AdminStatus"),
        ("/metrics", "AdminMetrics"),
        ("/admin/metrics/timeseries", "AdminMetricsTimeseries"),
    ] {
        assert_eq!(
            document["paths"][path]["get"]["responses"]["200"]["content"]["application/json"]
                ["schema"]["$ref"]
                .as_str(),
            Some(format!("#/components/schemas/{schema}").as_str())
        );
        assert_eq!(
            document["components"]["schemas"][schema]["additionalProperties"].as_bool(),
            Some(false)
        );
    }

    let windows = document["paths"]["/admin/metrics/timeseries"]["get"]["parameters"][0]["schema"]
        ["enum"]
        .as_sequence()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    assert_eq!(windows, vec!["1h", "12h", "24h", "7d"]);
    assert_eq!(
        document["paths"]["/admin/metrics/timeseries"]["get"]["responses"]["400"]["$ref"].as_str(),
        Some("#/components/responses/ValidationError")
    );

    let callback = &document["components"]["schemas"]["WebhookEvent"];
    assert_eq!(callback["additionalProperties"].as_bool(), Some(false));
    assert_eq!(
        required_fields(&document, "WebhookEvent"),
        ["event", "fencing_token", "holder_id", "lock", "timestamp"]
            .into_iter()
            .map(str::to_string)
            .collect()
    );
    let holder_description = callback["properties"]["holder_id"]["description"]
        .as_str()
        .unwrap();
    assert!(holder_description.contains("pseudonym"));
    assert!(holder_description.contains("never"));
}

#[test]
fn webhook_event_admission_matches_the_openapi_enum_exactly() {
    let document = document();
    let documented = document["components"]["schemas"]["CreateWebhookRequest"]["properties"]
        ["events"]["items"]["enum"]
        .as_sequence()
        .expect("webhook events need a finite enum")
        .iter()
        .map(|event| event.as_str().unwrap().to_string())
        .collect::<BTreeSet<_>>();
    let expected = ["*", "acquired", "expired", "released", "renewed"]
        .into_iter()
        .map(str::to_string)
        .collect::<BTreeSet<_>>();
    assert_eq!(documented, expected);

    let connection = rusqlite::Connection::open_in_memory().unwrap();
    let store = WebhookStore::new(Arc::new(Mutex::new(connection))).unwrap();
    let user_id = Uuid::new_v4();
    for (index, event) in expected.iter().enumerate() {
        store
            .create_webhook(
                user_id,
                CreateWebhookRequest {
                    url: format!("https://example.com/hooks/{index}"),
                    secret: None,
                    events: Some(vec![event.clone()]),
                    lock_pattern: None,
                },
            )
            .unwrap_or_else(|error| panic!("documented event {event} was rejected: {error}"));
    }
    assert!(store
        .create_webhook(
            user_id,
            CreateWebhookRequest {
                url: "https://example.com/hooks/invalid".to_string(),
                secret: None,
                events: Some(vec!["bogus".to_string()]),
                lock_pattern: None,
            },
        )
        .is_err());
    assert_eq!(store.get_user_webhooks(user_id).len(), expected.len());
}

#[test]
fn local_registration_contract_is_explicit_one_time_and_collision_safe() {
    let document = document();
    let operation = &document["paths"]["/auth/register"]["post"];
    let description = operation["description"]
        .as_str()
        .expect("local registration needs a security-boundary description");
    for required in [
        "Disabled by default",
        "LOCAL_REGISTRATION=true",
        "loopback",
        "unique case-insensitively",
        "Existing usernames fail with 409",
        "never return a stored token",
    ] {
        assert!(description.contains(required), "missing '{required}'");
    }
    assert_eq!(
        operation["responses"]["200"]["headers"]["Cache-Control"]["schema"]["enum"][0].as_str(),
        Some("no-store")
    );
    assert_eq!(
        operation["responses"]["409"]["$ref"].as_str(),
        Some("#/components/responses/ConflictError")
    );
}
