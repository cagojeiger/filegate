//! S3 호환 클라이언트 구성, 접근 검증, presign, 실물 조회.
//!
//! 입력은 등록부의 storage 행 + 복호된 시크릿이다 (spec 01).
//! 등록 시점과 부팅 재검증이 connect를, 도메인 오퍼레이션이
//! presign_put(발급)과 head_object(commit의 사후 검증)를 호출한다.

use std::time::Duration;

use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
use aws_sdk_s3::presigning::PresigningConfig;
use secrecy::{ExposeSecret, SecretString};

/// S3 호환 storage 접근 명세: 등록부 행 + 복호된 자격증명.
#[derive(Debug, Clone)]
pub struct S3StorageSpec {
    /// filegate 프로세스가 검증·실물 조회에 쓰는 내부 접근 주소.
    pub endpoint: String,
    /// 전송 주체가 presigned URL로 접근할 공개 주소. 같을 수 있지만 같은 개념은 아니다.
    pub public_endpoint: String,
    pub region: String,
    pub bucket: String,
    pub force_path_style: bool,
    pub access_key: String,
    pub secret_key: SecretString,
}

#[derive(Debug, Clone)]
pub struct S3Storage {
    pub client: aws_sdk_s3::Client,
    pub bucket: String,
}

/// 어느 주소로 클라이언트를 만들 것인가. SigV4는 호스트를 서명에 묶으므로,
/// presign은 전송 주체가 실제 접속할 공개 주소로 서명해야 한다 (spec 01).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Address {
    /// filegate 프로세스가 직접 부르는 경로 (검증, head_object).
    Internal,
    /// 전송 주체에게 건네질 URL의 서명 (presign).
    Public,
}

/// 접근 확인 없이 클라이언트만 구성한다. 요청 경로(presign·head_object)용 —
/// 접근성은 등록·부팅 재검증이 이미 보증했다.
pub fn client(spec: &S3StorageSpec, address: Address) -> S3Storage {
    let credentials = Credentials::new(
        spec.access_key.clone(),
        spec.secret_key.expose_secret().to_owned(),
        None,
        None,
        "filegate-registry",
    );
    let endpoint = match address {
        Address::Internal => &spec.endpoint,
        Address::Public => &spec.public_endpoint,
    };
    let s3_config = aws_sdk_s3::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new(spec.region.clone()))
        .endpoint_url(endpoint)
        .credentials_provider(credentials)
        .force_path_style(spec.force_path_style)
        .build();
    S3Storage {
        client: aws_sdk_s3::Client::from_conf(s3_config),
        bucket: spec.bucket.clone(),
    }
}

/// 클라이언트를 만들고 버킷에 접근 가능한지 확인한다.
///
/// filegate는 자기 버킷만 다룬다 — 버킷 프로비저닝은 운영자 몫이다. 버킷이
/// 없거나 접근 권한이 없으면 실패한다 (등록 거부 또는 부팅 중단, ADR 001).
/// head_bucket이 존재와 기본 접근을 함께 확인한다. (fs adapter는 경로 존재·
/// 쓰기 가능으로 같은 검증을 한다 — storage 모델마다 방식이 다르다.)
pub async fn connect(spec: &S3StorageSpec) -> anyhow::Result<S3Storage> {
    let storage = client(spec, Address::Internal);
    storage
        .client
        .head_bucket()
        .bucket(&storage.bucket)
        .send()
        .await
        .map_err(|err| {
            anyhow::anyhow!(
                "bucket '{}' not accessible at {} — provision it and grant access: {err}",
                spec.bucket,
                spec.endpoint
            )
        })?;
    Ok(storage)
}

/// 쓰기 lease의 실체 — 만료가 있는 presigned PUT URL (spec 00 create).
/// content_type을 선언하면 서명에 포함되어 강제된다 (실측). 크기는 앞단에서
/// 막지 못한다 — commit의 사후 검증이 게이트다.
pub async fn presign_put(
    storage: &S3Storage,
    object_key: &str,
    content_type: Option<&str>,
    expires_in: Duration,
) -> anyhow::Result<String> {
    let mut request = storage
        .client
        .put_object()
        .bucket(&storage.bucket)
        .key(object_key);
    if let Some(content_type) = content_type {
        request = request.content_type(content_type);
    }
    let presigned = request
        .presigned(PresigningConfig::expires_in(expires_in)?)
        .await?;
    Ok(presigned.uri().to_owned())
}

/// 읽기 lease의 실체 — 만료가 있는 presigned GET URL (spec 00 read).
/// filename을 주면 RFC 5987로 Content-Disposition에 실어 서명한다 (ADR 003,
/// 실측: 서명에 포함해야 강제된다).
pub async fn presign_get(
    storage: &S3Storage,
    object_key: &str,
    filename: Option<&str>,
    expires_in: Duration,
) -> anyhow::Result<String> {
    let mut request = storage
        .client
        .get_object()
        .bucket(&storage.bucket)
        .key(object_key);
    if let Some(filename) = filename {
        request = request.response_content_disposition(format!(
            "attachment; filename*=UTF-8''{}",
            rfc5987_encode(filename)
        ));
    }
    let presigned = request
        .presigned(PresigningConfig::expires_in(expires_in)?)
        .await?;
    Ok(presigned.uri().to_owned())
}

/// 응답 ETag를 따옴표 제거해 꺼낸다 — 없으면 벤더 규약 위반 에러.
/// (단일 PUT ETag는 MD5, multipart는 digest-of-digests `-N`, 실측.)
fn strip_etag(etag: Option<&str>, op: &str) -> anyhow::Result<String> {
    Ok(etag
        .ok_or_else(|| anyhow::anyhow!("{op} returned no etag"))?
        .trim_matches('"')
        .to_owned())
}

/// RFC 5987 value-chars 이외를 UTF-8 바이트 단위로 퍼센트 인코딩한다.
pub fn rfc5987_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        let attr_char = byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'!' | b'#' | b'$' | b'&' | b'+' | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~'
            );
        if attr_char {
            out.push(byte as char);
        } else {
            use std::fmt::Write;
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

/// 중계 쓰기의 뒷단 업로드 — 스풀 파일에서 스트리밍한다 (크기 기지).
/// filegate가 스트림 중 크기·MD5를 이미 검증했으므로 여기서는 전달만.
pub async fn put_object_from_path(
    storage: &S3Storage,
    object_key: &str,
    path: &std::path::Path,
    content_type: Option<&str>,
) -> anyhow::Result<()> {
    let body = aws_sdk_s3::primitives::ByteStream::from_path(path).await?;
    let mut request = storage
        .client
        .put_object()
        .bucket(&storage.bucket)
        .key(object_key)
        .body(body);
    if let Some(content_type) = content_type {
        request = request.content_type(content_type);
    }
    request.send().await?;
    Ok(())
}

/// 중계 읽기의 뒷단 스트림 — (AsyncRead, 크기). 없으면 None.
pub async fn open_read(
    storage: &S3Storage,
    object_key: &str,
) -> anyhow::Result<Option<(impl tokio::io::AsyncRead + Send + Unpin, i64)>> {
    let result = storage
        .client
        .get_object()
        .bucket(&storage.bucket)
        .key(object_key)
        .send()
        .await;
    match result {
        Ok(output) => {
            let len = output.content_length().unwrap_or(0);
            Ok(Some((output.body.into_async_read(), len)))
        }
        Err(error) => {
            let not_found = error
                .as_service_error()
                .map(|service| {
                    matches!(
                        service,
                        aws_sdk_s3::operation::get_object::GetObjectError::NoSuchKey(_)
                    )
                })
                .unwrap_or(false);
            if not_found {
                Ok(None)
            } else {
                Err(error.into())
            }
        }
    }
}

/// 물리 삭제 (reconciler의 purge·회수). S3 DeleteObject는 없는 키에도
/// 성공한다 — purge는 멱등하다 (spec 00, 실측).
pub async fn delete_object(storage: &S3Storage, object_key: &str) -> anyhow::Result<()> {
    storage
        .client
        .delete_object()
        .bucket(&storage.bucket)
        .key(object_key)
        .send()
        .await?;
    Ok(())
}

/// 실물 메타 조회 (commit의 사후 검증). 없으면 None.
/// 반환: (크기, ETag — 따옴표 제거. 단일 PUT이면 MD5와 같다, 실측).
pub async fn head_object(
    storage: &S3Storage,
    object_key: &str,
) -> anyhow::Result<Option<(i64, String)>> {
    let result = storage
        .client
        .head_object()
        .bucket(&storage.bucket)
        .key(object_key)
        .send()
        .await;
    match result {
        Ok(head) => {
            let size = head
                .content_length()
                .ok_or_else(|| anyhow::anyhow!("head_object returned no content length"))?;
            let etag = strip_etag(head.e_tag(), "head_object")?;
            Ok(Some((size, etag)))
        }
        Err(error) => {
            let not_found = error
                .as_service_error()
                .map(|service| service.is_not_found())
                .unwrap_or(false);
            if not_found {
                Ok(None)
            } else {
                Err(error.into())
            }
        }
    }
}

// ---- multipart (spec 02) ----

/// multipart 세션 시작 — 벤더 upload_id를 돌려준다. lease에 저장되어
/// 완성(Complete)·중단(Abort)의 핸들이 된다 (파생 불가능한 외부 값).
pub async fn create_multipart(
    storage: &S3Storage,
    object_key: &str,
    content_type: Option<&str>,
) -> anyhow::Result<String> {
    let mut request = storage
        .client
        .create_multipart_upload()
        .bucket(&storage.bucket)
        .key(object_key);
    if let Some(content_type) = content_type {
        request = request.content_type(content_type);
    }
    let output = request.send().await?;
    output
        .upload_id()
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("create_multipart returned no upload id"))
}

/// part 쓰기 presigned URL (직결 multipart). presign이므로 공개 주소
/// 클라이언트로 서명해야 한다 (spec 01 — SigV4는 호스트를 묶는다).
pub async fn presign_upload_part(
    storage: &S3Storage,
    object_key: &str,
    upload_id: &str,
    part_number: i32,
    expires_in: Duration,
) -> anyhow::Result<String> {
    let presigned = storage
        .client
        .upload_part()
        .bucket(&storage.bucket)
        .key(object_key)
        .upload_id(upload_id)
        .part_number(part_number)
        .presigned(PresigningConfig::expires_in(expires_in)?)
        .await?;
    Ok(presigned.uri().to_owned())
}

/// 중계 s3의 part 전달 — 스풀 파일에서 벤더 part로 (spec 02: 도착 즉시
/// 올리고 스풀을 지워 디스크 점유를 유계로). 반환: 벤더 part ETag.
pub async fn upload_part_from_path(
    storage: &S3Storage,
    object_key: &str,
    upload_id: &str,
    part_number: i32,
    path: &std::path::Path,
) -> anyhow::Result<String> {
    let body = aws_sdk_s3::primitives::ByteStream::from_path(path).await?;
    let output = storage
        .client
        .upload_part()
        .bucket(&storage.bucket)
        .key(object_key)
        .upload_id(upload_id)
        .part_number(part_number)
        .body(body)
        .send()
        .await?;
    strip_etag(output.e_tag(), "upload_part")
}

/// 벤더에 실재하는 part 목록 (직결 commit의 대조 재료): (번호, 크기, ETag).
/// 페이지네이션을 따른다 — part는 최대 10,000개다.
pub async fn list_parts(
    storage: &S3Storage,
    object_key: &str,
    upload_id: &str,
) -> anyhow::Result<Vec<(i32, i64, String)>> {
    let mut parts = Vec::new();
    let mut marker: Option<String> = None;
    loop {
        let mut request = storage
            .client
            .list_parts()
            .bucket(&storage.bucket)
            .key(object_key)
            .upload_id(upload_id);
        if let Some(marker) = &marker {
            request = request.part_number_marker(marker);
        }
        let output = request.send().await?;
        for part in output.parts() {
            let number = part
                .part_number()
                .ok_or_else(|| anyhow::anyhow!("list_parts returned a part without number"))?;
            let size = part
                .size()
                .ok_or_else(|| anyhow::anyhow!("list_parts returned a part without size"))?;
            let etag = part
                .e_tag()
                .ok_or_else(|| anyhow::anyhow!("list_parts returned a part without etag"))?
                .trim_matches('"')
                .to_owned();
            parts.push((number, size, etag));
        }
        if output.is_truncated() == Some(true) {
            let next = output.next_part_number_marker().map(str::to_owned);
            // 방어: 비표준 S3 호환 백엔드가 truncated인데 마커를 안 주거나
            // 전진시키지 않으면 첫 페이지를 영원히 재요청한다 — 전진 못 하면
            // 있는 만큼만 반환하고 멈춘다 (commit이 개수 불일치로 거부).
            if next.is_none() || next == marker {
                break;
            }
            marker = next;
        } else {
            break;
        }
    }
    Ok(parts)
}

/// 완성 — 검증된 part 목록으로 조립을 선언한다 (조립은 벤더 몫).
/// 반환: multipart ETag (digest-of-digests, `-N` 접미 — 전체 MD5가 아니다).
pub async fn complete_multipart(
    storage: &S3Storage,
    object_key: &str,
    upload_id: &str,
    parts: &[(i32, String)],
) -> anyhow::Result<String> {
    use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
    let completed = CompletedMultipartUpload::builder()
        .set_parts(Some(
            parts
                .iter()
                .map(|(number, etag)| {
                    CompletedPart::builder()
                        .part_number(*number)
                        .e_tag(etag)
                        .build()
                })
                .collect(),
        ))
        .build();
    let output = storage
        .client
        .complete_multipart_upload()
        .bucket(&storage.bucket)
        .key(object_key)
        .upload_id(upload_id)
        .multipart_upload(completed)
        .send()
        .await?;
    strip_etag(output.e_tag(), "complete_multipart")
}

/// 중단 — 미완성 part의 점유·과금을 제거한다 (회수 경로, spec 02).
/// 이미 없는 세션(NoSuchUpload)도 성공 — 회수는 멱등하다.
pub async fn abort_multipart(
    storage: &S3Storage,
    object_key: &str,
    upload_id: &str,
) -> anyhow::Result<()> {
    let result = storage
        .client
        .abort_multipart_upload()
        .bucket(&storage.bucket)
        .key(object_key)
        .upload_id(upload_id)
        .send()
        .await;
    match result {
        Ok(_) => Ok(()),
        Err(error) => {
            let gone = error
                .raw_response()
                .map(|response| response.status().as_u16() == 404)
                .unwrap_or(false);
            if gone {
                Ok(())
            } else {
                Err(error.into())
            }
        }
    }
}
