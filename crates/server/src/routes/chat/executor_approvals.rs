use axum::{
    Extension,
    extract::{Path, State},
    response::Json as ResponseJson,
};
use db::models::{
    chat_executor_approval_request::ChatExecutorApprovalRequest, chat_session::ChatSession,
};
use deployment::Deployment;
use serde::Deserialize;
use services::services::approvals::executor_approvals::ExecutorApprovalBridge;
use utils::response::ApiResponse;
use uuid::Uuid;

use crate::{DeploymentImpl, error::ApiError};

#[derive(Debug, Deserialize)]
pub struct ResolveExecutorApprovalRequest {
    pub option_id: String,
}

pub async fn list_pending(
    Extension(session): Extension<ChatSession>,
    State(deployment): State<DeploymentImpl>,
) -> Result<ResponseJson<ApiResponse<Vec<ChatExecutorApprovalRequest>>>, ApiError> {
    let requests = ExecutorApprovalBridge::list_pending(&deployment.db().pool, session.id).await?;
    Ok(ResponseJson(ApiResponse::success(requests)))
}

pub async fn resolve(
    Extension(session): Extension<ChatSession>,
    State(deployment): State<DeploymentImpl>,
    Path((_session_id, request_id)): Path<(Uuid, Uuid)>,
    ResponseJson(payload): ResponseJson<ResolveExecutorApprovalRequest>,
) -> Result<ResponseJson<ApiResponse<ChatExecutorApprovalRequest>>, ApiError> {
    let option_id = payload.option_id.trim();
    if option_id.is_empty() {
        return Err(ApiError::BadRequest("option_id is required".to_string()));
    }
    let resolved = ExecutorApprovalBridge::resolve(
        &deployment.db().pool,
        session.id,
        request_id,
        option_id,
        deployment.user_id(),
    )
    .await?
    .ok_or_else(|| {
        ApiError::Conflict(
            "approval is not pending, is expired, or the option is invalid".to_string(),
        )
    })?;
    Ok(ResponseJson(ApiResponse::success(resolved)))
}

#[cfg(test)]
mod tests {
    use axum::{
        Extension, Router,
        body::{Body, to_bytes},
        http::{Method, Request, StatusCode},
        routing::post,
    };
    use chrono::{TimeDelta, Utc};
    use db::{
        DBService,
        models::{
            chat_executor_approval_request::{
                ChatExecutorApprovalOption, ChatExecutorApprovalRequest,
                CreateChatExecutorApprovalRequest,
            },
            chat_session::ChatSession,
        },
    };
    use serde_json::{Value, json};
    use sqlx::{SqlitePool, sqlite::SqlitePoolOptions};
    use tower::ServiceExt;

    use super::*;

    struct Fixture {
        app: Router,
        pool: SqlitePool,
        session_id: Uuid,
        session_agent_id: Uuid,
        run_id: Uuid,
    }

    async fn setup_fixture() -> Fixture {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("connect route test database");
        sqlx::migrate!("../db/migrations")
            .run(&pool)
            .await
            .expect("run route test migrations");

        let session_id = Uuid::new_v4();
        let agent_id = Uuid::new_v4();
        let session_agent_id = Uuid::new_v4();
        let run_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO chat_sessions (id, title, status) VALUES (?1, 'route test', 'active')",
        )
        .bind(session_id)
        .execute(&pool)
        .await
        .expect("insert session");
        sqlx::query(
            "INSERT INTO chat_agents (id, name, runner_type) VALUES (?1, 'RouteQwen', 'QWEN_CODE')",
        )
        .bind(agent_id)
        .execute(&pool)
        .await
        .expect("insert agent");
        sqlx::query(
            "INSERT INTO chat_session_agents (id, session_id, agent_id, state) \
             VALUES (?1, ?2, ?3, 'waitingapproval')",
        )
        .bind(session_agent_id)
        .bind(session_id)
        .bind(agent_id)
        .execute(&pool)
        .await
        .expect("insert session agent");
        sqlx::query(
            "INSERT INTO chat_runs (id, session_id, session_agent_id, run_index, run_dir) \
             VALUES (?1, ?2, ?3, 1, '/tmp/qwen-route-test')",
        )
        .bind(run_id)
        .bind(session_id)
        .bind(session_agent_id)
        .execute(&pool)
        .await
        .expect("insert chat run");
        let session = ChatSession::find_by_id(&pool, session_id)
            .await
            .expect("load session")
            .expect("session exists");
        let deployment =
            local_deployment::LocalDeployment::new_for_test_pool(DBService { pool: pool.clone() })
                .await
                .expect("create test deployment");
        let app = Router::new()
            .route(
                "/sessions/{session_id}/approvals/{request_id}/resolve",
                post(super::resolve),
            )
            .layer(Extension(session))
            .with_state(deployment);

        Fixture {
            app,
            pool,
            session_id,
            session_agent_id,
            run_id,
        }
    }

    async fn create_request(fixture: &Fixture, tool_call_id: &str) -> Uuid {
        ChatExecutorApprovalRequest::create_or_find(
            &fixture.pool,
            &CreateChatExecutorApprovalRequest {
                session_id: fixture.session_id,
                session_agent_id: fixture.session_agent_id,
                run_id: fixture.run_id,
                workflow_execution_id: None,
                workflow_step_id: None,
                runner: "QWEN_CODE".to_string(),
                tool_call_id: tool_call_id.to_string(),
                tool_name: "write_file".to_string(),
                display_input: json!({"path": "approval.txt"}),
                options: vec![
                    ChatExecutorApprovalOption {
                        option_id: "proceed_once".to_string(),
                        kind: "allow_once".to_string(),
                        label: "Proceed once".to_string(),
                    },
                    ChatExecutorApprovalOption {
                        option_id: "cancel".to_string(),
                        kind: "reject_once".to_string(),
                        label: "Cancel".to_string(),
                    },
                ],
                expires_at: Utc::now() + TimeDelta::minutes(5),
            },
            Uuid::new_v4(),
        )
        .await
        .expect("create approval request")
        .id
    }

    async fn resolve_request(
        fixture: &Fixture,
        request_id: Uuid,
        option_id: &str,
    ) -> (StatusCode, Value) {
        let uri = format!(
            "/sessions/{}/approvals/{request_id}/resolve",
            fixture.session_id
        );
        let response = fixture
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&json!({"option_id": option_id}))
                            .expect("serialize resolve body"),
                    ))
                    .expect("build resolve request"),
            )
            .await
            .expect("execute resolve request");
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read resolve body");
        let body = serde_json::from_slice(&bytes).unwrap_or_else(|_| json!({}));
        (status, body)
    }

    #[tokio::test]
    async fn resolve_route_extracts_session_and_request_ids_and_preserves_conflicts() {
        let fixture = setup_fixture().await;
        let proceed = create_request(&fixture, "tool-proceed").await;
        let cancel = create_request(&fixture, "tool-cancel").await;
        let conflict = create_request(&fixture, "tool-conflict").await;

        let (proceed_status, proceed_body) =
            resolve_request(&fixture, proceed, "proceed_once").await;
        assert_eq!(proceed_status, StatusCode::OK, "{proceed_body}");
        assert_eq!(
            proceed_body["data"]["selected_option_id"],
            json!("proceed_once")
        );

        let (cancel_status, cancel_body) = resolve_request(&fixture, cancel, "cancel").await;
        assert_eq!(cancel_status, StatusCode::OK, "{cancel_body}");
        assert_eq!(cancel_body["data"]["selected_option_id"], json!("cancel"));

        let (invalid_status, invalid_body) = resolve_request(&fixture, conflict, "invented").await;
        assert_eq!(invalid_status, StatusCode::CONFLICT, "{invalid_body}");
        let (valid_status, valid_body) = resolve_request(&fixture, conflict, "proceed_once").await;
        assert_eq!(valid_status, StatusCode::OK, "{valid_body}");
        let (duplicate_status, duplicate_body) =
            resolve_request(&fixture, conflict, "proceed_once").await;
        assert_eq!(duplicate_status, StatusCode::CONFLICT, "{duplicate_body}");
    }
}
