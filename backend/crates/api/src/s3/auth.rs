//! S3 표면의 SigV4 인증 (spec 03) — header-signed 서명을 검증해 client_id를
//! 낸다. secret은 암호화 저장돼 있어(storage 벤더 시크릿과 같은 기계) 검증
//! 시 access_key_id를 AAD로 복호해 HMAC을 다시 계산한다 — 회전은 enc_key_id
//! 라벨 dispatch가 커버한다. 실패는 완성된 XML 403이다.

use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::Response;
use filegate_core::ExposeSecret as _;
use filegate_db::s3_registry as s3reg;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq as _;

use super::header_str;
use super::xml::{access_denied, xml_error, xml_internal};
use crate::routes::AppState;

/// SigV4 요청 시각의 허용 스큐 (AWS 관례 ±15분).
const MAX_CLOCK_SKEW_SECS: i64 = 15 * 60;

type HmacSha256 = Hmac<Sha256>;

fn hmac_sha256(key: &[u8], msg: &[u8]) -> Vec<u8> {
    // Hmac::new_from_slice는 임의 길이 키를 받아 InvalidLength가 나지 않는다.
    // 그래도 unwrap/expect는 워크스페이스 린트가 막으므로 let-else로 받는다 —
    // 이 분기는 도달 불가다. 설령 도달해도 결과는 실제 서명과 다른 값이라
    // 상수시간 비교에서 불일치(403)로 닫힌다.
    let Ok(mut mac) = <HmacSha256 as Mac>::new_from_slice(key) else {
        return Vec::new();
    };
    mac.update(msg);
    mac.finalize().into_bytes().to_vec()
}

fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

struct ParsedAuth {
    access_key: String,
    scope_date: String,
    region: String,
    service: String,
    terminator: String,
    signed_headers: Vec<String>,
    signature: String,
}

/// `AWS4-HMAC-SHA256 Credential=AK/date/region/s3/aws4_request,
///  SignedHeaders=h1;h2, Signature=hex`
fn parse_auth(auth: &str) -> Option<ParsedAuth> {
    let rest = auth.strip_prefix("AWS4-HMAC-SHA256 ")?;
    let mut credential = None;
    let mut signed = None;
    let mut signature = None;
    for part in rest.split(',') {
        let (name, value) = part.trim().split_once('=')?;
        match name {
            "Credential" => credential = Some(value),
            "SignedHeaders" => {
                signed = Some(value.split(';').map(str::to_owned).collect::<Vec<_>>())
            }
            "Signature" => signature = Some(value.to_owned()),
            _ => {}
        }
    }
    let mut scope = credential?.split('/');
    Some(ParsedAuth {
        access_key: scope.next()?.to_owned(),
        scope_date: scope.next()?.to_owned(),
        region: scope.next()?.to_owned(),
        service: scope.next()?.to_owned(),
        terminator: scope.next()?.to_owned(),
        signed_headers: signed?,
        signature: signature?,
    })
}

/// 쿼리스트링의 canonical form — 키 정렬. 지원 4개 오퍼레이션엔 쿼리가
/// 없어(실측) 대개 빈 문자열이지만, 서명은 요청 그대로 위에서 성립해야 한다.
fn canonicalize_query(query: &str) -> String {
    let mut pairs: Vec<(&str, &str)> = query
        .split('&')
        .filter(|s| !s.is_empty())
        .map(|p| p.split_once('=').unwrap_or((p, "")))
        .collect();
    pairs.sort_unstable();
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// SigV4 검증 → client_id. 실패는 완성된 XML 403이다.
///
/// canonical request의 URI는 요청 라인의 percent-encoded 경로 그대로다 —
/// 클라이언트가 서명한 바이트와 같아야 하므로 디코딩하지 않는다 (실측:
/// 유니코드 키는 인코딩된 채 도착). payload hash는 x-amz-content-sha256
/// 헤더 값을 그대로 canonical에 넣는다 — 본문 실검증은 PUT 스트림이 한다.
pub(super) async fn authenticate(
    state: &AppState,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
) -> Result<String, Response> {
    let auth = header_str(headers, "authorization")
        .ok_or_else(|| access_denied("missing authorization"))?;
    let parsed = parse_auth(auth).ok_or_else(|| access_denied("malformed authorization"))?;
    if parsed.service != "s3" || parsed.terminator != "aws4_request" {
        return Err(access_denied(
            "credential scope must be <date>/<region>/s3/aws4_request",
        ));
    }

    let amz_date =
        header_str(headers, "x-amz-date").ok_or_else(|| access_denied("missing x-amz-date"))?;
    if !amz_date.starts_with(parsed.scope_date.as_str()) {
        return Err(access_denied(
            "x-amz-date does not match the credential scope",
        ));
    }
    let request_time = chrono::NaiveDateTime::parse_from_str(amz_date, "%Y%m%dT%H%M%SZ")
        .map_err(|_| access_denied("malformed x-amz-date"))?
        .and_utc();
    if (chrono::Utc::now() - request_time).num_seconds().abs() > MAX_CLOCK_SKEW_SECS {
        return Err(xml_error(
            StatusCode::FORBIDDEN,
            "RequestTimeTooSkewed",
            "the difference between the request time and the server time is too large",
        ));
    }

    let payload_hash = header_str(headers, "x-amz-content-sha256")
        .ok_or_else(|| access_denied("missing x-amz-content-sha256"))?;
    if payload_hash.starts_with("STREAMING-") {
        // aws-chunked 스트리밍 서명은 청크 디코딩을 요구한다 — 보류 (spec 03).
        // boto3/봇오코어는 HTTP에서 실해시를 보낸다 (실측).
        return Err(xml_error(
            StatusCode::NOT_IMPLEMENTED,
            "NotImplemented",
            "streaming payload signatures are not supported",
        ));
    }

    let credential = s3reg::get_credential(&state.pool, &parsed.access_key)
        .await
        .map_err(|e| xml_internal("credential lookup", e))?
        .ok_or_else(|| {
            xml_error(
                StatusCode::FORBIDDEN,
                "InvalidAccessKeyId",
                "the access key id does not exist",
            )
        })?;
    // 암호화 저장된 secret을 복호한다 — storage 벤더 시크릿과 같은 기계.
    // enc_key_id 라벨이 복호 키를 고르므로(active·PREV) 회전이 자연히 커버되고,
    // AAD=access_key_id가 암호문 재배치를 막는다.
    let secret = state
        .crypto
        .decrypt(
            &credential.enc_key_id,
            &parsed.access_key,
            &filegate_core::EncryptedSecret {
                ciphertext: credential.secret_ciphertext,
                nonce: credential.secret_nonce,
            },
        )
        .map_err(|e| xml_internal("secret decrypt", e))?;

    // canonical request — SignedHeaders 목록 순서대로 (소문자:trim값).
    let mut canonical_headers = String::new();
    for name in &parsed.signed_headers {
        let value =
            header_str(headers, name).ok_or_else(|| access_denied("signed header absent"))?;
        canonical_headers.push_str(name);
        canonical_headers.push(':');
        canonical_headers.push_str(value.trim());
        canonical_headers.push('\n');
    }
    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        method.as_str(),
        uri.path(),
        uri.query().map(canonicalize_query).unwrap_or_default(),
        canonical_headers,
        parsed.signed_headers.join(";"),
        payload_hash,
    );
    let scope = format!("{}/{}/s3/aws4_request", parsed.scope_date, parsed.region);
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );

    let k_date = hmac_sha256(
        format!("AWS4{}", secret.expose_secret()).as_bytes(),
        parsed.scope_date.as_bytes(),
    );
    let k_region = hmac_sha256(&k_date, parsed.region.as_bytes());
    let k_service = hmac_sha256(&k_region, b"s3");
    let k_signing = hmac_sha256(&k_service, b"aws4_request");
    let expected = hex::encode(hmac_sha256(&k_signing, string_to_sign.as_bytes()));

    // 서명 비교는 상수 시간 (config.rs 연산자 토큰 대조와 같은 프리미티브).
    if bool::from(expected.as_bytes().ct_eq(parsed.signature.as_bytes())) {
        return Ok(credential.client_id);
    }
    Err(xml_error(
        StatusCode::FORBIDDEN,
        "SignatureDoesNotMatch",
        "the request signature does not match",
    ))
}
