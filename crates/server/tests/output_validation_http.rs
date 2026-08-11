use axum::http::StatusCode;
use db::DBService;
use serde_json::{Value, json};

async fn start_server() -> (String, tokio::task::JoinHandle<()>) {
    let pool = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("create in-memory database");
    sqlx::migrate!("../db/migrations")
        .run(&pool)
        .await
        .expect("run migrations");
    let deployment = local_deployment::LocalDeployment::new_for_test_pool(DBService { pool })
        .await
        .expect("create local deployment");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind HTTP listener");
    let address = listener.local_addr().expect("read listener address");
    let server = tokio::spawn(async move {
        axum::serve(listener, server::routes::router(deployment))
            .await
            .expect("serve output validation API");
    });
    (format!("http://{address}"), server)
}

async fn post_validation(
    client: &reqwest::Client,
    base_url: &str,
    body: Value,
) -> (StatusCode, Value) {
    let response = client
        .post(format!("{base_url}/api/output-validation"))
        .json(&body)
        .send()
        .await
        .expect("send output validation request");
    let status = response.status();
    let body = response.json().await.expect("parse validation response");
    (status, body)
}

fn valid_plan() -> Value {
    json!({
        "version": "1",
        "title": "Implement validation",
        "goal": "Validate every model JSON protocol before it is returned",
        "agents": {
            "lead": "lead",
            "available": ["lead", "worker"]
        },
        "nodes": [
            {
                "id": "implementation",
                "type": "workflowStep",
                "data": {
                    "stepType": "task",
                    "agentId": "worker",
                    "title": "Implement",
                    "instructions": "Implement the validator and its tests.",
                    "acceptance": { "required": ["All tests pass"] },
                    "outputs": ["crates/services/src/services/output_validation.rs"],
                    "selfCheck": ["Review all protocol variants"],
                    "verificationCommands": ["cargo test -p server --test output_validation_http"],
                    "completionEvidence": ["Passing test output"]
                }
            },
            {
                "id": "result",
                "type": "workflowStep",
                "data": {
                    "stepType": "result",
                    "agentId": "lead",
                    "title": "Result",
                    "instructions": "Summarize the verified implementation."
                }
            }
        ],
        "edges": [
            {
                "id": "implementation-result",
                "source": "implementation",
                "target": "result",
                "data": { "kind": "hard" }
            }
        ]
    })
}

fn assert_valid_response(status: StatusCode, body: &Value, kind: &str) {
    assert_eq!(status, StatusCode::OK, "response body: {body}");
    assert_eq!(body["valid"], true, "response body: {body}");
    assert_eq!(body["kind"], kind, "response body: {body}");
    assert_eq!(body["errors"], json!([]), "response body: {body}");
}

fn assert_invalid_response(status: StatusCode, body: &Value, kind: &str) {
    assert_eq!(status, StatusCode::OK, "response body: {body}");
    assert_eq!(body["valid"], false, "response body: {body}");
    assert_eq!(body["kind"], kind, "response body: {body}");
    assert!(
        body["errors"]
            .as_array()
            .is_some_and(|errors| !errors.is_empty()),
        "response body: {body}"
    );
}

#[tokio::test]
async fn validates_all_model_output_protocols_over_real_http() {
    let (base_url, server) = start_server().await;
    let client = reqwest::Client::new();
    let execution_id = uuid::Uuid::new_v4();

    let (status, body) = post_validation(
        &client,
        &base_url,
        json!({
            "kind": "chat_protocol",
            "output": [
                {"type": "send", "to": "you", "intent": "confirm", "content": "Done"},
                {"type": "artifact", "content": ["crates/services/src/services/output_validation.rs"]},
                {"type": "conclusion", "content": "Validation completed."}
            ],
            "context": { "allowed_targets": ["you", "Backend"] }
        }),
    )
    .await;
    assert_valid_response(status, &body, "chat_protocol");

    let workflow_chat_output = json!([
        {"type": "send", "to": "you", "content": "The design is ready."},
        {"type": "workflow_generate", "plan_check": true, "content": "Implement output validation."}
    ])
    .to_string();
    let (status, body) = post_validation(
        &client,
        &base_url,
        json!({
            "kind": "chat_workflow_protocol",
            "output": workflow_chat_output,
            "context": {
                "allowed_targets": ["you"],
                "workflow_generation_allowed": true
            }
        }),
    )
    .await;
    assert_valid_response(status, &body, "chat_workflow_protocol");

    let (status, body) = post_validation(
        &client,
        &base_url,
        json!({
            "kind": "workflow_plan",
            "output": valid_plan(),
            "context": {
                "lead_agent_id": "lead",
                "available_agent_ids": ["lead", "worker"]
            }
        }),
    )
    .await;
    assert_valid_response(status, &body, "workflow_plan");

    let (status, body) = post_validation(
        &client,
        &base_url,
        json!({
            "kind": "workflow_task",
            "output": {
                "type": "final_result",
                "step_key": "implementation",
                "execution_id": execution_id,
                "status": "done",
                "summary": "Implemented and verified",
                "content": "The endpoint validates every protocol.",
                "verification": [
                    {"name": "HTTP integration", "command": "cargo test", "status": "passed", "evidence": "all cases passed"}
                ],
                "files_changed": ["crates/services/src/services/output_validation.rs"],
                "self_review": ["Reviewed request dispatch and error mapping"],
                "issues": [],
                "evidence": ["integration test output"],
                "outputs": ["crates/services/src/services/output_validation.rs"]
            },
            "context": {
                "execution_id": execution_id,
                "step_key": "implementation",
                "allow_interaction_requests": true
            }
        }),
    )
    .await;
    assert_valid_response(status, &body, "workflow_task");

    let criteria = json!([
        {"id": "c1", "level": "required", "criterion": "All tests pass"}
    ]);
    let (status, body) = post_validation(
        &client,
        &base_url,
        json!({
            "kind": "workflow_step_review",
            "output": {
                "type": "review_result",
                "step_key": "implementation",
                "execution_id": execution_id,
                "summary": "The implementation passes review.",
                "results": {"c1": {"passed": true, "evidence": "integration tests passed"}}
            },
            "context": {
                "execution_id": execution_id,
                "step_key": "implementation",
                "criteria": criteria
            }
        }),
    )
    .await;
    assert_valid_response(status, &body, "workflow_step_review");

    let (status, body) = post_validation(
        &client,
        &base_url,
        json!({
            "kind": "workflow_loop_review",
            "output": {
                "type": "loop_review_result",
                "loop_key": "validation-loop",
                "execution_id": execution_id,
                "summary": "The loop is complete.",
                "results": {"c1": {"passed": true, "evidence": "all scoped work passed"}},
                "rework": {}
            },
            "context": {
                "execution_id": execution_id,
                "loop_key": "validation-loop",
                "criteria": criteria,
                "allowed_step_keys": ["implementation"]
            }
        }),
    )
    .await;
    assert_valid_response(status, &body, "workflow_loop_review");

    let invalid_requests = [
        json!({
            "kind": "chat_protocol",
            "output": [
                {"type": "send", "to": "you", "content": "one"},
                {"type": "send", "to": "user", "content": "two"}
            ],
            "context": { "allowed_targets": ["you"] }
        }),
        json!({
            "kind": "chat_workflow_protocol",
            "output": [{"type": "workflow_generate", "plan_check": true, "content": "start"}],
            "context": {"allowed_targets": ["you"], "workflow_generation_allowed": false}
        }),
        json!({
            "kind": "workflow_plan",
            "output": {"unexpected": true},
            "context": {"lead_agent_id": "lead", "available_agent_ids": ["lead", "worker"]}
        }),
        json!({
            "kind": "workflow_task",
            "output": {"type": "error", "step_key": "wrong", "execution_id": execution_id, "message": "failed"},
            "context": {"execution_id": execution_id, "step_key": "implementation", "allow_interaction_requests": true}
        }),
        json!({
            "kind": "workflow_step_review",
            "output": {"type": "review_result", "step_key": "implementation", "execution_id": execution_id, "summary": "missing result", "results": {}},
            "context": {"execution_id": execution_id, "step_key": "implementation", "criteria": criteria}
        }),
        json!({
            "kind": "workflow_loop_review",
            "output": {"type": "loop_review_result", "loop_key": "validation-loop", "execution_id": execution_id, "summary": "rejected", "results": {"c1": {"passed": false, "evidence": "failed"}}, "rework": {}},
            "context": {"execution_id": execution_id, "loop_key": "validation-loop", "criteria": criteria, "allowed_step_keys": ["implementation"]}
        }),
    ];
    let invalid_kinds = [
        "chat_protocol",
        "chat_workflow_protocol",
        "workflow_plan",
        "workflow_task",
        "workflow_step_review",
        "workflow_loop_review",
    ];
    for (request, kind) in invalid_requests.into_iter().zip(invalid_kinds) {
        let (status, body) = post_validation(&client, &base_url, request).await;
        assert_invalid_response(status, &body, kind);
    }

    let (status, body) = post_validation(
        &client,
        &base_url,
        json!({"kind": "unknown", "output": {}, "context": {}}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "response body: {body}");

    server.abort();
    let _ = server.await;
}
