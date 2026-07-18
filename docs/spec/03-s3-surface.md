# spec 03: S3 호환 표면 오퍼레이션

- Status: Accepted
- Date: 2026-07-14
- 근거: ADR [006](../adr/006-s3-compat-surface.md) (중계를 수용한 온보딩 계층), [002](../adr/002-lease-model.md) (모든 접근은 lease), [003](../adr/003-url-ownership.md) (논리키 = 서비스 소유 이름)
- 실측: boto3/botocore (2026-07-14, [scripts/s3-capture.py](../../scripts/s3-capture.py)) — 아래 요청 모양은 관측값이다

무수정 S3 SDK가 filegate를 상대로 동작하는 표면의 계약을 정한다. 이
표면의 바이트는 업로드·다운로드 모두 filegate를 지난다 (ADR 006이
수용한 비용). 파일·lease·회계는 네이티브 표면과 한 장부다.

## 지원 오퍼레이션 — 이것이 전부다

| 오퍼레이션 | 요청 | 성공 | 주요 실패 |
|---|---|---|---|
| PutObject | `PUT /{bucket}/{key}` | 200, `ETag: "<md5>"` | 403(서명)·404(bucket)·400(본문) |
| HeadObject | `HEAD /{bucket}/{key}` | 200 + Content-Length·Content-Type·ETag | 404 |
| GetObject | `GET /{bucket}/{key}` (+`Range`) | 200 스트림 / 206 부분 | 404·416 |
| DeleteObject | `DELETE /{bucket}/{key}` | 204 — 멱등 | 403 |

넷으로 충분함이 실측이다: boto3의 put→head→get(+Range)→delete→head(404)
전체 수명에서 **다른 요청은 하나도 나가지 않는다** — ListBuckets·
HeadBucket 같은 프로브가 없다.

## 결정 사항

### 주소와 어휘

- 표면은 **전용 리스너**(`FILEGATE_S3_BIND`)에 산다 — S3 path-style은
  루트 경로가 bucket이라, 컨트롤 표면(`/api`·`/blobs`·probes)과 자리
  경합을 리스너 분리로 없앤다.
- path-style만 지원한다: `/{bucket}/{key}`. virtual-host style은 보류.
- **bucket = intent.** 등록된 binding이 없으면 404 `NoSuchBucket`.
  서비스는 운영자가 통보한 기본 버킷 이름 하나만 안다 (ADR 006).
- **key = 논리키** — 서비스 소유 이름 (ADR 003). 퍼센트 인코딩·유니코드를
  수용하고 디코딩된 형태로 보관한다. 같은 키 재PUT은 덮어쓰기다:
  매핑이 새 file을 가리키고 옛 file은 detach로 넘어간다 — S3의
  덮어쓰기 시맨틱을 상태 기계로 번역한 것.

### 인증 — SigV4

- 자격증명은 access key id + secret key 쌍이다. 등록부(운영자 API)가
  client에 발급하며, bearer 클라이언트 키와 별개다. access key id는 공개
  식별자, secret은 고엔트로피 랜덤이고 발급 응답에서 원문이 딱 한 번 나간다.
- **secret은 암호화 저장한다** — storage 벤더 시크릿과 같은 기계 (재현이
  필요한 장수 시크릿은 암호화 저장, 찰나인 relay만 파생). SigV4 검증은
  access_key_id를 AAD로 복호해 raw로 HMAC을 재계산한다. 마스터 키 회전은
  `enc_key_id` 라벨 dispatch가 커버하고(spec 01 런북, storages와 함께),
  유출 반경은 저장된 암호문에 국한되며 자격증명은 행 단위로 폐기·회전한다.
- **header-signed SigV4를 검증한다.** canonical request의 payload hash는
  `x-amz-content-sha256` 헤더 값을 그대로 쓴다 — 실측: PUT은 실제
  본문 SHA256, GET/HEAD/DELETE는 empty-payload hash.
- `SignedHeaders`에 열거된 헤더 전부가 canonical에 들어간다 — boto3
  기본 무결성 헤더(`x-amz-checksum-crc32`, `x-amz-sdk-checksum-algorithm`)도
  서명 범위로 함께 검증된다 (실측: 최신 botocore가 PUT에 기본 첨부).
- **query-signed(presigned URL)도 검증한다.** 서명·자격이 쿼리스트링
  (`X-Amz-Credential/Date/Expires/SignedHeaders/Signature`)에 실려 오고,
  canonical query에서 `X-Amz-Signature`만 제외해 재계산한다. payload hash는
  `UNSIGNED-PAYLOAD`, 만료는 `X-Amz-Expires` 창으로 본다. 서비스가 자기 S3
  SDK의 `generate_presigned_url`을 filegate에 그대로 겨누는 경로다.

### 전송과 검증

- PUT은 `Expect: 100-continue`를 수용한다 (실측: boto3 기본 첨부).
  스트림 실측(크기·MD5)이 검증 재료고 **완료 즉시 확정한다** — 관찰
  확정(spec 00)과 같은 게이트이며 별도 commit이 없다. S3에도 없으므로
  대칭이다. ETag = 실측 MD5, 따옴표 포함.
- GET은 단일 구간 `Range: bytes=a-b`를 지원한다 — 206/416. boto3
  `download_file`(병렬 Range 다운로드)의 전제다.
- checksum 재계산(CRC32 대조)은 하지 않는다 — 무결성은 크기·MD5 실측이
  담당하고, checksum 헤더는 서명 검증의 일부로만 쓰인다.
- 모든 접근은 lease 원장을 지난다 (ADR 002) — 표면이 내부적으로 lease를
  만들어 관찰·회계·이력이 네이티브와 한 장부가 된다.

### 에러 모양

S3 표준 XML 최소형으로 답한다 — SDK가 이걸 파싱한다:

```xml
<Error><Code>NoSuchKey</Code><Message>…</Message></Error>
```

Code 어휘는 S3 표준을 따른다: `NoSuchBucket`, `NoSuchKey`,
`SignatureDoesNotMatch`, `AccessDenied`, `InvalidRange`.

## 다음 범위로 미룬다

- **multipart 4종** (CreateMultipartUpload/UploadPart/Complete/Abort) —
  boto3 `upload_file`의 8MiB 초과 자동 전환이 요구한다. 그 전까지는
  가이드가 단일 PUT을 고정한다 ([가이드](../guide/s3-onboarding.md)).
- **ListObjectsV2** — 보류. 목록의 진실 원천은 서비스 DB다 (ADR 003).
- CopyObject·bucket 계열 오퍼레이션 — 계획 없음.

## 완료 기준

[scripts/s3-capture.py](../../scripts/s3-capture.py)가 endpoint만 바꿔
MinIO와 filegate에서 **동일하게 통과한다** — 표면 동등성의 실측 정의다.
