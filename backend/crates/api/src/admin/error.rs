use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use filegate_db::registry::{self, WriteOp, WriteViolation};

pub(super) fn error_response(status: StatusCode, message: &str) -> Response {
    (status, Json(serde_json::json!({ "error": message }))).into_response()
}

pub(super) fn not_found(message: &str) -> Response {
    error_response(StatusCode::NOT_FOUND, message)
}

pub(super) fn internal_error(error: filegate_core::Error) -> Response {
    tracing::error!(event = "admin.internal", %error);
    error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
}

pub(super) fn db_error_response(error: &filegate_db::DbError, op: WriteOp) -> Response {
    match registry::write_violation(error, op) {
        Some(WriteViolation::Duplicate) => error_response(StatusCode::CONFLICT, "already exists"),
        Some(WriteViolation::MissingRef(constraint)) => {
            // 없는 부모를 가리키는 쓰기 — 어느 노드가 없는지 제약 이름이 말해준다.
            let target = if constraint.contains("storage") {
                "storage not found"
            } else if constraint.contains("client") {
                "client not found"
            } else {
                "referenced registration not found"
            };
            not_found(target)
        }
        Some(WriteViolation::InUse) => error_response(
            StatusCode::CONFLICT,
            "still referenced — delete bindings/files first",
        ),
        Some(WriteViolation::Invalid) => error_response(
            StatusCode::BAD_REQUEST,
            "invalid field (id slug, capacity_bytes >= 0, key hash format)",
        ),
        None => {
            tracing::error!(event = "admin.db_error", %error);
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "database error")
        }
    }
}
