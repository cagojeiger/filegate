//! filegate 클라이언트 — 컨트롤 플레인 API와 바이트 전송을 감싼다.
//!
//! api 크레이트의 두 번째 소비자다 (첫째는 E2E 셸). 서비스가 실제로 쓰는
//! 방식 그대로 계약을 호출한다: create → (PUT | parts+PUT) → commit → read → GET.
//! 실패 케이스 검증을 위해 4xx를 에러로 바꾸지 않고 (status, body)로 돌려준다.

use anyhow::Result;
use md5::{Digest, Md5};

/// 컨트롤 플레인 응답 — 상태 코드와 파싱된 본문.
pub struct Outcome {
    pub status: u16,
    pub body: serde_json::Value,
}

impl Outcome {
    pub fn str(&self, key: &str) -> Option<&str> {
        self.body.get(key).and_then(|v| v.as_str())
    }
    pub fn i64(&self, key: &str) -> Option<i64> {
        self.body.get(key).and_then(|v| v.as_i64())
    }
    /// multipart 서술자 (있으면 multipart 업로드).
    pub fn multipart(&self) -> Option<(i64, i64)> {
        let m = self.body.get("multipart")?;
        Some((
            m.get("part_size")?.as_i64()?,
            m.get("part_count")?.as_i64()?,
        ))
    }
}

/// 바이트 GET 결과 — 상태, 본문, Content-Disposition.
pub struct GetResult {
    pub status: u16,
    pub bytes: Vec<u8>,
    pub disposition: Option<String>,
}

pub struct FilegateClient {
    base: String,
    /// None = Authorization 헤더 없음. Some("") = 빈 Bearer (인증 실패 케이스용).
    key: Option<String>,
    http: reqwest::Client,
}

impl FilegateClient {
    pub fn new(base: impl Into<String>, key: impl Into<String>) -> Self {
        Self {
            base: base.into().trim_end_matches('/').to_owned(),
            key: Some(key.into()),
            http: reqwest::Client::new(),
        }
    }
    /// 인증 헤더 없이 (또는 잘못된 키로) — 401 케이스 검증용.
    pub fn with_key(base: impl Into<String>, key: Option<String>) -> Self {
        Self {
            base: base.into().trim_end_matches('/').to_owned(),
            key,
            http: reqwest::Client::new(),
        }
    }

    fn auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.key {
            Some(k) => req.header("Authorization", format!("Bearer {k}")),
            None => req,
        }
    }

    async fn control(&self, req: reqwest::RequestBuilder) -> Result<Outcome> {
        let resp = self.auth(req).send().await?;
        let status = resp.status().as_u16();
        let text = resp.text().await?;
        let body = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
        Ok(Outcome { status, body })
    }

    /// create — 쓰기 lease 발급 (spec 00). declared_md5는 단일 PUT에서만 쓴다.
    pub async fn create(
        &self,
        intent: &str,
        declared_size: i64,
        content_type: Option<&str>,
        declared_md5: Option<&str>,
    ) -> Result<Outcome> {
        let mut map = serde_json::Map::new();
        map.insert("intent".into(), intent.into());
        map.insert("declared_size".into(), declared_size.into());
        if let Some(ct) = content_type {
            map.insert("content_type".into(), ct.into());
        }
        if let Some(md5) = declared_md5 {
            map.insert("declared_md5".into(), md5.into());
        }
        let body = serde_json::Value::Object(map);
        self.control(
            self.http
                .post(format!("{}/v1/files", self.base))
                .json(&body),
        )
        .await
    }

    /// parts — part 접근 발급 = 갱신 = 재개 (spec 02). 반환에서 URL을 꺼내 쓴다.
    pub async fn parts(&self, file_id: &str, nums: &[i32]) -> Result<Outcome> {
        let body = serde_json::json!({ "parts": nums });
        self.control(
            self.http
                .post(format!("{}/v1/files/{file_id}/parts", self.base))
                .json(&body),
        )
        .await
    }

    /// 발급된 parts 응답에서 특정 part의 URL을 꺼낸다.
    pub fn part_url(out: &Outcome, part: i32) -> Option<String> {
        out.body.get("parts")?.as_array()?.iter().find_map(|p| {
            (p.get("part")?.as_i64()? == i64::from(part))
                .then(|| p.get("url")?.as_str().map(str::to_owned))
                .flatten()
        })
    }

    pub async fn commit(&self, file_id: &str) -> Result<Outcome> {
        self.control(
            self.http
                .post(format!("{}/v1/files/{file_id}/commit", self.base)),
        )
        .await
    }

    pub async fn read(&self, file_id: &str, filename: Option<&str>) -> Result<Outcome> {
        let body = match filename {
            Some(f) => serde_json::json!({ "filename": f }),
            None => serde_json::json!({}),
        };
        self.control(
            self.http
                .post(format!("{}/v1/files/{file_id}/read", self.base))
                .json(&body),
        )
        .await
    }

    pub async fn stat(&self, file_id: &str) -> Result<Outcome> {
        self.control(self.http.get(format!("{}/v1/files/{file_id}", self.base)))
            .await
    }

    pub async fn delete(&self, file_id: &str) -> Result<u16> {
        let resp = self
            .auth(
                self.http
                    .delete(format!("{}/v1/files/{file_id}", self.base)),
            )
            .send()
            .await?;
        Ok(resp.status().as_u16())
    }

    /// 바이트 PUT — presigned URL 또는 /b. URL이 인증을 담으므로 헤더 없음.
    /// 길이 기지 본문이라 Content-Length가 붙는다. 반환: (status, ETag).
    pub async fn put(
        &self,
        url: &str,
        bytes: Vec<u8>,
        content_type: Option<&str>,
    ) -> Result<(u16, Option<String>)> {
        let mut req = self.http.put(url).body(bytes);
        if let Some(ct) = content_type {
            req = req.header("Content-Type", ct);
        }
        let resp = req.send().await?;
        let status = resp.status().as_u16();
        let etag = resp
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.trim_matches('"').to_owned());
        Ok((status, etag))
    }

    /// Content-Length 없는 PUT (스트림 본문 → chunked). 길이 미상 거부 검증용.
    pub async fn put_chunked(&self, url: &str, bytes: Vec<u8>) -> Result<u16> {
        let stream = futures_stream_once(bytes);
        let resp = self
            .http
            .put(url)
            .body(reqwest::Body::wrap_stream(stream))
            .send()
            .await?;
        Ok(resp.status().as_u16())
    }

    pub async fn get(&self, url: &str) -> Result<GetResult> {
        let resp = self.http.get(url).send().await?;
        let status = resp.status().as_u16();
        let disposition = resp
            .headers()
            .get("content-disposition")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let bytes = resp.bytes().await?.to_vec();
        Ok(GetResult {
            status,
            bytes,
            disposition,
        })
    }

    // ---- 고수준 흐름 (사용자 도구용) ----

    /// 파일 하나를 올린다 — 서술자를 보고 단일/multipart 자동 선택.
    /// 성공하면 file_id를 돌려준다.
    pub async fn upload_file(
        &self,
        path: &str,
        intent: &str,
        content_type: Option<&str>,
    ) -> Result<String> {
        let bytes = tokio::fs::read(path).await?;
        let size = bytes.len() as i64;
        let created = self.create(intent, size, content_type, None).await?;
        if created.status != 201 {
            anyhow::bail!("create failed: {} {}", created.status, created.body);
        }
        let file_id = created
            .str("file_id")
            .ok_or_else(|| anyhow::anyhow!("no file_id"))?
            .to_owned();

        if let Some((part_size, part_count)) = created.multipart() {
            let nums: Vec<i32> = (1..=part_count as i32).collect();
            let issued = self.parts(&file_id, &nums).await?;
            for n in &nums {
                let start = ((n - 1) as i64 * part_size) as usize;
                let end = (start + part_size as usize).min(bytes.len());
                let url = Self::part_url(&issued, *n)
                    .ok_or_else(|| anyhow::anyhow!("no url for part {n}"))?;
                let chunk = bytes.get(start..end).unwrap_or_default().to_vec();
                let (code, _) = self.put(&url, chunk, None).await?;
                anyhow::ensure!(code == 200, "part {n} PUT {code}");
            }
        } else {
            let url = created
                .str("put_url")
                .ok_or_else(|| anyhow::anyhow!("no put_url"))?;
            let (code, _) = self.put(url, bytes, content_type).await?;
            anyhow::ensure!(code == 200, "PUT {code}");
        }
        let committed = self.commit(&file_id).await?;
        anyhow::ensure!(committed.status == 200, "commit {}", committed.status);
        Ok(file_id)
    }

    /// 파일을 내려받아 저장한다.
    pub async fn download_file(
        &self,
        file_id: &str,
        out: &str,
        filename: Option<&str>,
    ) -> Result<()> {
        let read = self.read(file_id, filename).await?;
        anyhow::ensure!(read.status == 200, "read {}", read.status);
        let url = read
            .str("get_url")
            .ok_or_else(|| anyhow::anyhow!("no get_url"))?;
        let got = self.get(url).await?;
        anyhow::ensure!(got.status == 200, "GET {}", got.status);
        tokio::fs::write(out, got.bytes).await?;
        Ok(())
    }
}

/// 바이트 벡터 하나를 내보내는 스트림 (chunked 본문용).
fn futures_stream_once(
    bytes: Vec<u8>,
) -> impl futures_util::Stream<Item = Result<Vec<u8>, std::io::Error>> {
    futures_util::stream::once(async move { Ok(bytes) })
}

pub fn md5_hex(bytes: &[u8]) -> String {
    let mut hasher = Md5::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}
