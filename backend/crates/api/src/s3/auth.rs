//! S3 표면의 SigV4 인증 (spec 03) — header-signed와 query-signed(presigned)
//! 서명을 검증해 client_id를 낸다. 두 경로는 "서명을 어디서 읽나"(헤더 vs
//! 쿼리)와 payload·만료 검사만 다르고, canonical request 조립·서명 재계산은
//! 완전히 공통이다. secret은 암호화 저장돼 있어(storage 벤더 시크릿과 같은
//! 기계) access_key_id를 AAD로 복호해 HMAC을 다시 계산한다 — 회전은
//! enc_key_id 라벨 dispatch가 커버한다. 실패는 완성된 XML이다.

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

/// SigV4 요청 시각의 허용 스큐 (AWS 관례 ±15분). presigned는 여기에 더해
/// X-Amz-Expires 창 안이어야 한다.
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

/// 검증에 필요한 재료 — 두 서명 모드가 같은 형태로 모은다. 여기까지 오면
/// 이후 조립·재계산은 모드와 무관하다.
struct SigV4 {
    access_key: String,
    scope_date: String,
    region: String,
    service: String,
    terminator: String,
    signed_headers: Vec<String>,
    signature: String,
    /// string-to-sign에 들어갈 시각 (header: x-amz-date, query: X-Amz-Date).
    amz_date: String,
    /// canonical request의 payload hash (header: x-amz-content-sha256,
    /// query(presigned): 언제나 UNSIGNED-PAYLOAD).
    payload_hash: String,
    /// canonical query — 받은 인코딩 보존, X-Amz-Signature 제외.
    canonical_query: String,
}

/// `AWS4-HMAC-SHA256 Credential=AK/date/region/s3/aws4_request,
///  SignedHeaders=h1;h2, Signature=hex`
struct ParsedAuth {
    access_key: String,
    scope_date: String,
    region: String,
    service: String,
    terminator: String,
    signed_headers: Vec<String>,
    signature: String,
}

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

/// 쿼리 파라미터의 raw(인코딩된) 값. canonical query는 받은 인코딩 그대로
/// 써야 서명이 성립하므로 디코딩하지 않는다.
fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query
        .split('&')
        .filter_map(|p| p.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v)
}

/// canonical query — 키 정렬. X-Amz-Signature는 제외한다: 어느 서명 모드든
/// 서명 자신은 canonical에 들어가지 않는다(presigned의 핵심 규칙). 받은
/// percent-encoding을 그대로 보존한다 — 서명한 바이트와 같아야 하므로.
fn canonicalize_query(query: &str) -> String {
    let mut pairs: Vec<(&str, &str)> = query
        .split('&')
        .filter(|s| !s.is_empty())
        .map(|p| p.split_once('=').unwrap_or((p, "")))
        .filter(|(k, _)| *k != "X-Amz-Signature")
        .collect();
    pairs.sort_unstable();
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// header-signed 재료 — Authorization 헤더 + x-amz-* 헤더에서.
// Err=Response는 s3 표면의 관용구(authenticate와 같음) — sync fn이라만 lint 대상.
#[allow(clippy::result_large_err)]
fn from_header(uri: &Uri, headers: &HeaderMap) -> Result<SigV4, Response> {
    let auth = header_str(headers, "authorization")
        .ok_or_else(|| access_denied("missing authorization"))?;
    let parsed = parse_auth(auth).ok_or_else(|| access_denied("malformed authorization"))?;

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

    Ok(SigV4 {
        access_key: parsed.access_key,
        scope_date: parsed.scope_date,
        region: parsed.region,
        service: parsed.service,
        terminator: parsed.terminator,
        signed_headers: parsed.signed_headers,
        signature: parsed.signature,
        amz_date: amz_date.to_owned(),
        payload_hash: payload_hash.to_owned(),
        canonical_query: uri.query().map(canonicalize_query).unwrap_or_default(),
    })
}

/// query-signed(presigned) 재료 — 서명·자격이 쿼리스트링에 있다. payload는
/// UNSIGNED-PAYLOAD로 고정, 만료는 X-Amz-Expires 창으로 검사한다. 이게 서비스가
/// 자기 S3 SDK의 `generate_presigned_url`을 filegate에 그대로 겨누는 경로다.
#[allow(clippy::result_large_err)]
fn from_query(uri: &Uri) -> Result<SigV4, Response> {
    let query = uri.query().unwrap_or_default();
    let algorithm = query_param(query, "X-Amz-Algorithm")
        .ok_or_else(|| access_denied("missing X-Amz-Algorithm"))?;
    if algorithm != "AWS4-HMAC-SHA256" {
        return Err(access_denied("unsupported signing algorithm"));
    }

    // X-Amz-Credential은 `/`가 %2F로 인코딩돼 온다 — scope 파싱용으로만 디코딩.
    let credential = query_param(query, "X-Amz-Credential")
        .ok_or_else(|| access_denied("missing X-Amz-Credential"))?
        .replace("%2F", "/")
        .replace("%2f", "/");
    let mut scope = credential.split('/');
    let access_key = scope.next().unwrap_or_default().to_owned();
    let scope_date = scope.next().unwrap_or_default().to_owned();
    let region = scope.next().unwrap_or_default().to_owned();
    let service = scope.next().unwrap_or_default().to_owned();
    let terminator = scope.next().unwrap_or_default().to_owned();

    let amz_date =
        query_param(query, "X-Amz-Date").ok_or_else(|| access_denied("missing X-Amz-Date"))?;
    if !amz_date.starts_with(scope_date.as_str()) {
        return Err(access_denied(
            "X-Amz-Date does not match the credential scope",
        ));
    }
    let request_time = chrono::NaiveDateTime::parse_from_str(amz_date, "%Y%m%dT%H%M%SZ")
        .map_err(|_| access_denied("malformed X-Amz-Date"))?
        .and_utc();
    let expires: i64 = query_param(query, "X-Amz-Expires")
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| access_denied("missing or invalid X-Amz-Expires"))?;
    let elapsed = (chrono::Utc::now() - request_time).num_seconds();
    if elapsed < -MAX_CLOCK_SKEW_SECS {
        return Err(access_denied("the presigned url is not yet valid"));
    }
    if elapsed > expires {
        return Err(xml_error(
            StatusCode::FORBIDDEN,
            "AccessDenied",
            "the presigned url has expired",
        ));
    }

    let signed_headers = query_param(query, "X-Amz-SignedHeaders")
        .ok_or_else(|| access_denied("missing X-Amz-SignedHeaders"))?
        .replace("%3B", ";")
        .replace("%3b", ";")
        .split(';')
        .map(str::to_owned)
        .collect();
    let signature = query_param(query, "X-Amz-Signature")
        .ok_or_else(|| access_denied("missing X-Amz-Signature"))?
        .to_owned();

    Ok(SigV4 {
        access_key,
        scope_date,
        region,
        service,
        terminator,
        signed_headers,
        signature,
        amz_date: amz_date.to_owned(),
        payload_hash: "UNSIGNED-PAYLOAD".to_owned(),
        canonical_query: canonicalize_query(query),
    })
}

/// SigV4 검증 → client_id. 실패는 완성된 XML 403이다.
///
/// canonical request의 URI는 요청 라인의 percent-encoded 경로 그대로다 —
/// 클라이언트가 서명한 바이트와 같아야 하므로 디코딩하지 않는다 (실측:
/// 유니코드 키는 인코딩된 채 도착).
pub(super) async fn authenticate(
    state: &AppState,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
) -> Result<String, Response> {
    // 서명 위치로 모드를 가른다: Authorization 헤더면 header-signed,
    // 쿼리에 X-Amz-Signature가 있으면 presigned.
    let sig = if header_str(headers, "authorization").is_some() {
        from_header(uri, headers)?
    } else if uri.query().is_some_and(|q| q.contains("X-Amz-Signature=")) {
        from_query(uri)?
    } else {
        return Err(access_denied("missing authorization"));
    };

    if sig.service != "s3" || sig.terminator != "aws4_request" {
        return Err(access_denied(
            "credential scope must be <date>/<region>/s3/aws4_request",
        ));
    }

    let credential = s3reg::get_credential(&state.pool, &sig.access_key)
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
            &sig.access_key,
            &filegate_core::EncryptedSecret {
                ciphertext: credential.secret_ciphertext,
                nonce: credential.secret_nonce,
            },
        )
        .map_err(|e| xml_internal("secret decrypt", e))?;

    // canonical request — SignedHeaders 목록 순서대로 (소문자:trim값).
    let mut canonical_headers = String::new();
    for name in &sig.signed_headers {
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
        sig.canonical_query,
        canonical_headers,
        sig.signed_headers.join(";"),
        sig.payload_hash,
    );
    let scope = format!("{}/{}/s3/aws4_request", sig.scope_date, sig.region);
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{scope}\n{}",
        sig.amz_date,
        sha256_hex(canonical_request.as_bytes())
    );

    let k_date = hmac_sha256(
        format!("AWS4{}", secret.expose_secret()).as_bytes(),
        sig.scope_date.as_bytes(),
    );
    let k_region = hmac_sha256(&k_date, sig.region.as_bytes());
    let k_service = hmac_sha256(&k_region, b"s3");
    let k_signing = hmac_sha256(&k_service, b"aws4_request");
    let expected = hex::encode(hmac_sha256(&k_signing, string_to_sign.as_bytes()));

    // 서명 비교는 상수 시간 (config.rs 연산자 토큰 대조와 같은 프리미티브).
    if bool::from(expected.as_bytes().ct_eq(sig.signature.as_bytes())) {
        return Ok(credential.client_id);
    }
    Err(xml_error(
        StatusCode::FORBIDDEN,
        "SignatureDoesNotMatch",
        "the request signature does not match",
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn query_param_reads_the_value_or_none() {
        let q = "X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Signature=abc&X-Amz-Expires=900";
        assert_eq!(query_param(q, "X-Amz-Signature"), Some("abc"));
        assert_eq!(query_param(q, "X-Amz-Expires"), Some("900"));
        assert_eq!(query_param(q, "X-Amz-Missing"), None);
    }

    #[test]
    fn canonicalize_query_sorts_keys_and_drops_the_signature() {
        // 서명 자신은 canonical에서 빠지고, 나머지는 키 정렬 + 받은 인코딩 보존.
        let q = "X-Amz-Signature=zzz&X-Amz-Date=20260715T000000Z&X-Amz-Credential=AK%2F20260715%2Fauto%2Fs3%2Faws4_request";
        let c = canonicalize_query(q);
        assert!(!c.contains("X-Amz-Signature"));
        assert_eq!(
            c,
            "X-Amz-Credential=AK%2F20260715%2Fauto%2Fs3%2Faws4_request&X-Amz-Date=20260715T000000Z"
        );
    }

    #[test]
    fn from_query_parses_credential_scope_and_defaults_unsigned_payload() {
        // 유효 창 안의 최근 시각으로 만든 presigned 쿼리 (서명값은 형태만).
        let now = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
        let date = &now[..8];
        let uri: Uri = format!(
            "/b/k?X-Amz-Algorithm=AWS4-HMAC-SHA256\
             &X-Amz-Credential=fgak0123456789abcdef%2F{date}%2Fauto%2Fs3%2Faws4_request\
             &X-Amz-Date={now}&X-Amz-Expires=900&X-Amz-SignedHeaders=host&X-Amz-Signature=deadbeef"
        )
        .parse()
        .unwrap();
        let sig = from_query(&uri).expect("valid presigned query parses");
        assert_eq!(sig.access_key, "fgak0123456789abcdef");
        assert_eq!(sig.region, "auto");
        assert_eq!(sig.service, "s3");
        assert_eq!(sig.terminator, "aws4_request");
        assert_eq!(sig.signed_headers, vec!["host".to_owned()]);
        assert_eq!(sig.payload_hash, "UNSIGNED-PAYLOAD");
        assert_eq!(sig.signature, "deadbeef");
    }

    #[test]
    fn from_query_rejects_expired_url() {
        // X-Amz-Date가 만료창 훨씬 전이면 거부된다.
        let uri: Uri = "/b/k?X-Amz-Algorithm=AWS4-HMAC-SHA256\
             &X-Amz-Credential=ak%2F20200101%2Fauto%2Fs3%2Faws4_request\
             &X-Amz-Date=20200101T000000Z&X-Amz-Expires=900&X-Amz-SignedHeaders=host&X-Amz-Signature=x"
            .parse()
            .unwrap();
        assert!(from_query(&uri).is_err());
    }
}
