# MinIO (자가호스팅)

조사일: 2026-07-03

로컬 개발 환경의 기본 provider (docker-compose). 프로덕션에서는 홈서버의 ephemeral/개발/개인 벌크 용도 후보.

## 비용

- 한계비용 0 — 이미 돌리는 하드웨어의 디스크·전기·회선
- egress는 집 회선 업로드 대역폭이 물리적 한계
- 내구성·가용성·백업이 전부 내 책임 (벤더의 11-nines 없음)

## 상태 주의

- ⚠️ **2026년 초 OSS 레포 아카이브, 커뮤니티 에디션 기능 축소.** 기존 이미지는 로컬 개발용으로 충분하나 신규 기능·패치는 기대 불가
- 대체 후보: **Garage** (가볍고 진짜 OSS, presigned 지원), RustFS, SeaweedFS
- 우리 설계에서 교체 비용 = docker-compose 이미지 + config provider 블록 (ADR 001이 산 이유)

## S3 호환 사용법

- presigned GET/PUT/HEAD/DELETE, multipart 전부 지원
- path-style 필수 (`force_path_style: true`)
- docker-compose에서 presigned 서명 주의: 컨테이너 내부 주소(`minio:9000`)와 호스트 접근 주소(`localhost:9000`)가 달라 `endpoint` / `public_endpoint` 분리가 필요 (ADR 001의 내부/공개 주소 구분이 나온 배경)

## 배치 성격

- L0 ephemeral, 개발, 실험— 지워져도 아프지 않은 것들
- 홈서버 디스크만큼 capacity를 크게 잡을 수 있는 유일한 provider

## 출처

- https://github.com/minio/minio (아카이브 상태)
- https://garagehq.deuxfleurs.fr/
