//! OpenAPI 3.0 schema validation policy (F1.2).
//!
//! Spins up the proxy in front of a mock upstream and confirms that
//! valid bodies pass through, invalid bodies are rejected with the
//! configured status, out-of-scope routes are forwarded unchanged,
//! and `mode: log` warns instead of blocking.

use sbproxy_e2e::{MockUpstream, ProxyHarness};
use serde_json::json;

fn enforce_yaml(upstream: &str) -> String {
    format!(
        r#"
proxy:
  http_bind_port: 0
origins:
  "api.localhost":
    action:
      type: proxy
      url: "{upstream}"
    policies:
      - type: openapi_validation
        mode: enforce
        status: 422
        spec:
          openapi: "3.0.3"
          info: {{title: t, version: "1"}}
          paths:
            "/users/{{id}}":
              post:
                requestBody:
                  required: true
                  content:
                    application/json:
                      schema:
                        type: object
                        required: [name]
                        additionalProperties: false
                        properties:
                          name: {{type: string, minLength: 1}}
                          age:  {{type: integer, minimum: 0, maximum: 150}}
"#
    )
}

fn log_yaml(upstream: &str) -> String {
    format!(
        r#"
proxy:
  http_bind_port: 0
origins:
  "api.localhost":
    action:
      type: proxy
      url: "{upstream}"
    policies:
      - type: openapi_validation
        mode: log
        spec:
          openapi: "3.0.3"
          info: {{title: t, version: "1"}}
          paths:
            "/users/{{id}}":
              post:
                requestBody:
                  required: true
                  content:
                    application/json:
                      schema:
                        type: object
                        required: [name]
                        properties:
                          name: {{type: string}}
"#
    )
}

#[test]
fn valid_body_passes_through() {
    let upstream = MockUpstream::start(json!({"ok": true})).expect("upstream");
    let harness =
        ProxyHarness::start_with_yaml(&enforce_yaml(&upstream.base_url())).expect("start proxy");
    let resp = harness
        .post_json(
            "/users/42",
            "api.localhost",
            &json!({"name": "alice", "age": 30}),
            &[],
        )
        .expect("send");
    assert_eq!(resp.status, 200);
    let captured = upstream.captured();
    assert_eq!(captured.len(), 1, "upstream should see exactly one request");
}

#[test]
fn missing_required_field_is_rejected() {
    let upstream = MockUpstream::start(json!({"ok": true})).expect("upstream");
    let harness =
        ProxyHarness::start_with_yaml(&enforce_yaml(&upstream.base_url())).expect("start proxy");
    let resp = harness
        .post_json("/users/42", "api.localhost", &json!({"age": 30}), &[])
        .expect("send");
    assert_eq!(resp.status, 422);
    let text = resp.text().expect("utf-8");
    assert!(
        text.contains("openapi validation failed"),
        "expected validation error body, got: {text}"
    );
    // The proxy connects to the upstream before validation finishes,
    // so the upstream may see the request line + headers. What we
    // care about is that the rejected body is not forwarded.
    let captured = upstream.captured();
    if let Some(req) = captured.first() {
        assert!(
            req.body.is_empty() || !std::str::from_utf8(&req.body).unwrap_or("").contains("age"),
            "rejected body must not be forwarded upstream, got: {:?}",
            std::str::from_utf8(&req.body).unwrap_or("<bytes>")
        );
    }
}

// WOR-2687: the header-phase policy dispatcher runs before the body
// is buffered, so `OpenApiValidationEnforcer::enforce` always returns
// `Allow` there (see `builtin_enforcers::openapi_validation`) and the
// bus gets a `policy_verdict_event` saying "allow" for this policy_id
// before the request body has even arrived. Left uncorrected, that is
// the only record this request's `openapi_validation` decision ever
// gets, regardless of the 422 the client receives. Once the buffered
// validator in `request_body_filter` finds the real violation, it
// must publish a second, correcting event so a consumer reading the
// audit trail in publish order sees "deny" as the last word for this
// policy_id on this request.
#[test]
fn missing_required_field_publishes_a_deny_verdict() {
    let upstream = MockUpstream::start(json!({"ok": true})).expect("upstream");
    let harness =
        ProxyHarness::start_with_yaml(&enforce_yaml(&upstream.base_url())).expect("start proxy");
    let resp = harness
        .post_json("/users/42", "api.localhost", &json!({"age": 30}), &[])
        .expect("send");
    assert_eq!(resp.status, 422);

    // The audit bus drains asynchronously (see
    // `sbproxy_core::policy_bus::drain_to_stderr`), so a line can land
    // a beat after the HTTP response does. Two events are expected
    // for this request's `openapi_validation` policy_id: the
    // header-phase dispatcher's premature "allow" (the body doesn't
    // exist yet at that phase) and this fix's corrective "deny".
    // Poll until both have actually been drained to stderr rather
    // than stopping at the first one to appear, which would race
    // ahead of the second and read as a false failure.
    let openapi_line = |line: &str| {
        line.contains("policy_verdict_event")
            && line.contains("\"policy_id\":\"openapi_validation\"")
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut stderr = harness.stderr_contents();
    while stderr.lines().filter(|l| openapi_line(l)).count() < 2
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(std::time::Duration::from_millis(50));
        stderr = harness.stderr_contents();
    }

    let openapi_lines: Vec<&str> = stderr.lines().filter(|l| openapi_line(l)).collect();
    assert!(
        openapi_lines.len() >= 2,
        "expected the premature header-phase \"allow\" plus a corrective \
         \"deny\" for openapi_validation, got {} matching line(s), stderr: {stderr}",
        openapi_lines.len()
    );
    let last = openapi_lines.last().expect("checked len above");
    assert!(
        last.contains("\"verdict\":\"deny\""),
        "the last openapi_validation policy_verdict_event for a rejected \
         request must report \"deny\", got: {last}\nfull stderr: {stderr}"
    );
}

#[test]
fn additional_property_is_rejected() {
    let upstream = MockUpstream::start(json!({"ok": true})).expect("upstream");
    let harness =
        ProxyHarness::start_with_yaml(&enforce_yaml(&upstream.base_url())).expect("start proxy");
    let resp = harness
        .post_json(
            "/users/42",
            "api.localhost",
            &json!({"name": "alice", "rogue": "field"}),
            &[],
        )
        .expect("send");
    assert_eq!(resp.status, 422);
}

#[test]
fn out_of_scope_path_passes() {
    let upstream = MockUpstream::start(json!({"ok": true})).expect("upstream");
    let harness =
        ProxyHarness::start_with_yaml(&enforce_yaml(&upstream.base_url())).expect("start proxy");
    let resp = harness
        .post_json(
            "/widgets/42",
            "api.localhost",
            &json!({"anything": "goes"}),
            &[],
        )
        .expect("send");
    assert_eq!(resp.status, 200);
}

#[test]
fn log_mode_does_not_block_invalid_bodies() {
    let upstream = MockUpstream::start(json!({"ok": true})).expect("upstream");
    let harness =
        ProxyHarness::start_with_yaml(&log_yaml(&upstream.base_url())).expect("start proxy");
    let resp = harness
        .post_json("/users/42", "api.localhost", &json!({"age": 30}), &[])
        .expect("send");
    assert_eq!(resp.status, 200);
    let captured = upstream.captured();
    assert_eq!(
        captured.len(),
        1,
        "log mode must forward invalid bodies upstream"
    );
}
