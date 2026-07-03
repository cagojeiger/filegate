# OCI Object Storage

조사일: 2026-07-03

v0의 프로덕션 provider. S3 Compatibility API로만 사용한다(네이티브 PAR, 네이티브 lifecycle, Archive 자동화는 쓰지 않음 — ADR 001).

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

## filegate 설정 감각

```yaml
providers:
  oci-std:
    endpoint: "https://{namespace}.compat.objectstorage.ap-osaka-1.oraclecloud.com"
    public_endpoint: 동일 (공개 인터넷)
    region: "ap-osaka-1"
    force_path_style: true
capacity:
  oci-std: { max_total_bytes: 21474836480 } # 20 GiB = Always Free 상한
```

## 출처

- https://docs.oracle.com/en-us/iaas/Content/Object/Tasks/s3compatibleapi.htm
- https://blogs.oracle.com/cloud-infrastructure/post/how-to-use-aws-s3-presigned-urls-with-oci-object-storage
- https://www.oracle.com/cloud/free/
- https://ocispecialists.com/blog/oci-storage-pricing-guide/
- https://fullmetalbrackets.com/blog/oci-free-tier-breakdown
