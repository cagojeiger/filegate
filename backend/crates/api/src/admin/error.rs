//! 핸들러 에러 → HTTP 응답 번역. 핸들러는 `Result<_, ApiError>`를 돌려주고
//! `?`로 전파한다 — 상태 코드 규칙이 이 파일 한 곳에 산다.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use filegate_db::registry::{self, WriteOp, WriteViolation};

pub(super) enum ApiError {
    /// 명시적 상태와 메시지 (400/401/404).
    Status(StatusCode, String),
    /// DB 쓰기 거부 — 분류는 IntoResponse에서 (중복 409, 참조 없음 404,
    /// 사용 중 409, CHECK 위반 400).
    Db(filegate_db::DbError, WriteOp),
    /// 내부 실패 — 상세는 로그로, 응답은 일반 문구.
    Internal(filegate_core::Error),
}

pub(super) fn bad_request(message: &str) -> ApiError {
    ApiError::Status(StatusCode::BAD_REQUEST, message.to_owned())
}

pub(super) fn not_found(message: &str) -> ApiError {
    ApiError::Status(StatusCode::NOT_FOUND, message.to_owned())
}

pub(super) fn unauthorized() -> ApiError {
    ApiError::Status(
        StatusCode::UNAUTHORIZED,
        "operator token required".to_owned(),
    )
}

impl ApiError {
    /// DELETE 경로의 DB 에러 — FK 위반을 "참조가 남아 삭제 불가"(409)로 읽는다.
    /// 나머지 경로는 `From`(Insert 방향: 참조 대상 없음 = 404)이 담당한다.
    pub(super) fn on_delete(error: filegate_db::DbError) -> Self {
        Self::Db(error, WriteOp::Delete)
    }
}

impl From<filegate_db::DbError> for ApiError {
    fn from(error: filegate_db::DbError) -> Self {
        Self::Db(error, WriteOp::Insert)
    }
}

impl From<filegate_core::Error> for ApiError {
    fn from(error: filegate_core::Error) -> Self {
        Self::Internal(error)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            Self::Status(status, message) => payload(status, &message),
            Self::Db(error, op) => match registry::write_violation(&error, op) {
                Some(WriteViolation::Duplicate) => payload(StatusCode::CONFLICT, "already exists"),
                Some(WriteViolation::MissingRef(constraint)) => {
                    // 없는 부모를 가리키는 쓰기 — 어느 노드가 없는지 제약 이름이 말해준다.
                    let target = if constraint.contains("storage") {
                        "storage not found"
                    } else if constraint.contains("client") {
                        "client not found"
                    } else {
                        "referenced registration not found"
                    };
                    payload(StatusCode::NOT_FOUND, target)
                }
                Some(WriteViolation::InUse) => payload(
                    StatusCode::CONFLICT,
                    "still referenced — delete bindings/files first",
                ),
                Some(WriteViolation::Invalid) => payload(
                    StatusCode::BAD_REQUEST,
                    "invalid field (id slug, capacity_bytes >= 0, key hash format)",
                ),
                None => {
                    tracing::error!(event = "admin.db_error", %error);
                    payload(StatusCode::INTERNAL_SERVER_ERROR, "database error")
                }
            },
            Self::Internal(error) => {
                tracing::error!(event = "admin.internal", %error);
                payload(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
            }
        }
    }
}

fn payload(status: StatusCode, message: &str) -> Response {
    (status, Json(serde_json::json!({ "error": message }))).into_response()
}
