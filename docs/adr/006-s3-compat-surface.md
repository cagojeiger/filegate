# ADR 006: S3 호환 표면은 중계를 수용한 온보딩 계층이다

- Status: Accepted
- Date: 2026-07-14 (개정 2026-08-26: client 버킷 모델과 multipart 구현 반영)
- 부모: [005](005-presigned-byte-plane.md) (네이티브 바이트 평면), [002](002-lease-model.md), [003](003-url-ownership.md)

## 문제

네이티브 계약(ADR 005)은 바이트 직결과 검증을 가졌지만, "서비스가
filegate를 또 하나의 S3로 등록한다"는 온보딩 — 무수정 AWS SDK — 을
배제했다. 실측이 보여준 대가(표준 SDK는 GetObject 307을 따라가지
않는다 → 다운로드 직결 오프로드 불가) 때문이다. 그러나 온보딩 요구는
실재한다: 기존 S3 코드를 가진 서비스는 endpoint와 자격증명만 바꾸면
붙는 경험을 원한다. 대가를 숨기지 않고 **계층으로 명시**해 이 문을 연다.

## 결정

> **바이트 인터페이스는 두 계층이다.** 네이티브(presigned URL 발급,
> 바이트 직결 — 기본이자 비용 계층, ADR 005)와 **S3 호환(무수정 S3
> SDK, 바이트는 업로드·다운로드 모두 filegate를 지난다 — 온보딩
> 계층)**. 두 계층의 파일은 한 장부(files·leases·usage)다.

- **호환 표면은 양방향 중계다 — 이것이 수용한 비용이다.** S3 GetObject
  계약은 "응답 본문 = 파일 내용"이고 표준 SDK는 307을 따라가지 않는다
  (ADR 005 실측). 같은 네트워크(온프레 NAS·MinIO)에서 중계 비용은
  대역폭뿐이므로 v0 스코프에서 수용한다. 이그레스가 비싼 배치는
  네이티브 계층이 답이다 — 같은 장부 위에서 서비스 단위로 갈아탄다.
- **인증은 S3의 모양 그대로다.** access key id + secret key 쌍, 요청은
  SigV4로 검증한다. bearer 클라이언트 키와 별개 발급물이다. 검증 코드는
  스파이크(`spike/s3-gateway`)에서 승격했다.
- **bucket = client_id, object key = 서비스의 논리키.** 인증된 client는
  자기 id와 같은 버킷 하나만 쓴다. 서비스가 정한 논리키는 서비스 소유고
  등록부가 (client, 논리키) → file 매핑을 보관한다 (ADR 003).
- **단일 PUT은 관찰 확정, multipart는 Complete가 확정점이다.** 단일 PUT은
  중계 스트림 실측으로 즉시 확정한다. multipart는 S3 프로토콜 자체의
  CompleteMultipartUpload에서 part 원장을 검증하고 확정한다.
- **배치는 client의 단일 storage다.** 업로드는 client의 `storage_id`가
  가리키는 storage로 간다. 용량·비용 관리는 create 시점 선택이 아니라 **사후
  재배치(tiering, 다음 범위)**의 몫이다 — file/location 분리(ADR 001)가
  그 이동을 예비하고, 위치가 바뀌어도 논리키·file_id는 흔들리지 않는다.

## 범위

지원 범위: PutObject·GetObject·HeadObject·DeleteObject와 multipart 4종
(CreateMultipartUpload·UploadPart·CompleteMultipartUpload·AbortMultipartUpload),
SigV4 인증, 논리키 매핑. ListObjectsV2는 보류한다 — 목록의 진실 원천은
서비스 DB다.

## 경계선

- 호환 표면의 오퍼레이션 계약(에러 코드·헤더·응답 모양)은 spec 03이
  정의한다 (구현과 함께).
- 네이티브 계약(ADR 005)은 변하지 않는다 — 그 개정은 "제공하지
  않는다"가 "이 ADR의 계층으로 제공한다"로 바뀐 것뿐이다.
- 호환 표면의 접근도 lease 원장을 지난다 (ADR 002) — 표면이 달라도
  추적·회계는 하나다.

## 결과

- 서비스 온보딩 = "S3 계정 하나 더": endpoint + key 쌍 + 버킷 이름.
- 트래픽 비용의 선택권이 서비스 단위로 생긴다: 편의(호환) ↔ 직결(네이티브).
- SigV4 검증(header·query-signed), 논리키 매핑, 단일 객체 4종과 multipart
  4종을 구현했다 (spec 03, api/src/s3).
