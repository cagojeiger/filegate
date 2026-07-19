# OCI Object Storage

조사일: 2026-07-03

다음 범위의 프로덕션 후보 storage. v0는 MinIO + 로컬 fs이고, OCI는 이후에 붙인다 (spec 00). S3 Compatibility API로만 사용한다(네이티브 PAR, 네이티브 lifecycle, Archive 자동화는 쓰지 않음 — ADR 001).

## 비용

- Standard: ~$0.0255/GB·월 — ⚠️ 출처마다 표기가 갈림($0.0255 vs $0.0026 — 후자는 Archive 단가와 혼동으로 보임). 과금 구간 진입 전 [공식 price list](https://www.oracle.com/cloud/price-list/) 재확인 필요
- Infrequent Access: ~$0.01/GB·월
- Archive: $0.0026/GB·월 (Standard의 약 1/10). 복원 1~4시간 + 복원 ~$0.01/GB
- **Egress: 월 10TB 무료** (타사 100GB 대비 100배), 이후 ~$0.0085/GB. Ingress 무료
- 요청: 월 5만 회 무료, 이후 소액

## Always Free (영구 무료)

- Object Storage **20GB** + Archive **20GB** (별도 풀)
- 월 10TB egress 포함
- ⚠️ 1인 1테넌시 원칙. 유휴 리소스 회수 정책 있음 — 무료 구간 계정 스택은 ToS 회색지대

## S3 호환 사용법

- endpoint: `https://{namespace}.compat.objectstorage.{region}.oraclecloud.com`
  - namespace는 콘솔 프로필 → Tenancy 정보에서 확인
- 인증: **Customer Secret Key** (콘솔에서 발급, 유저당 최대 2개). SigV4 전용
- 버킷은 홈 리전의 기본 namespace에 있어야 compat API로 접근 가능
- presigned GET/PUT/HEAD/DELETE 지원, multipart 지원, presigned POST도 최근 추가
- path-style 권장 (`force_path_style: true`)
- ⚠️ **버킷 단위 CORS 설정 API가 없음** — 브라우저 직접 PUT(위임 업로드)이 성립하는지 M3에서 실측 필요. PAR 응답에 `access-control-allow-origin`이 온다는 커뮤니티 보고는 있으나 origin 제어 불가. 안 되면 폴백: filegate 중계 모드(ADR 002)

## filegate 설정 감각

운영자 API로 등록한다 (`POST /api/admin/v1/storages`) — YAML 설정 파일은 없다:

```json
{
  "id": "oci-std",
  "kind": "s3",
  "endpoint": "https://{namespace}.compat.objectstorage.ap-osaka-1.oraclecloud.com",
  "region": "ap-osaka-1",
  "bucket": "oci-std",
  "force_path_style": true,
  "access_key": "<Customer Secret Key id>",
  "secret_key": "<Customer Secret Key>",
  "capacity_bytes": 21474836480
}
```

`capacity_bytes`는 20 GiB = Always Free 상한. `public_endpoint`는 생략하면
endpoint와 같은 값으로 등록된다 (공개 인터넷 동일 주소).

## 출처

- https://docs.oracle.com/en-us/iaas/Content/Object/Tasks/s3compatibleapi.htm
- https://blogs.oracle.com/cloud-infrastructure/post/how-to-use-aws-s3-presigned-urls-with-oci-object-storage
- https://www.oracle.com/cloud/free/
- https://ocispecialists.com/blog/oci-storage-pricing-guide/
- https://fullmetalbrackets.com/blog/oci-free-tier-breakdown
