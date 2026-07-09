# spec 01: 등록부와 운영자 제어

- Status: Draft
- Date: 2026-07-09 (선언형 파일 설정안을 대체 — 등록부는 DB로, ADR 004 개정판 기준)
- 근거: ADR [000](../adr/000-identity.md), [001](../adr/001-multi-provider.md), [004](../adr/004-config-layers.md)

등록부의 모양, 검증 시점, v0 배치 규칙, 운영자 제어의 방향만 정한다. 테이블 필드·엔드포인트 세부는 구현이 확정한다.

## 등록부

- 정본은 DB다. 연결은 한 방향이다: `clients.intents → profiles → providers`.
- id는 운영자가 정하는 안정 슬러그다 (`oci-std`, `notegate`). 생성 후 불변 — CLI·Terraform·API 모두 이 id로 참조한다.
- 검증은 쓰기 시점이다: 참조 무결성은 FK가, provider 등록은 제출된 자격증명으로 저장 공간 접근을 즉석 확인해 지킨다. 실패한 등록은 거부된다.
- 부팅은 등록된 provider들의 접근을 재검증한다. 실패하면 부팅 중단 (ADR 001).

## 키와 비밀

- **운영자 토큰**: `FILEGATE_OPERATOR_TOKENS`(env, 쉼표 목록). 목록 중 하나와 일치하면 인증(상수시간 비교). 로테이션 = 새 토큰을 서브로 추가 → 클라이언트(TF·MCP) 전환 → 옛 토큰 제거. 무중단.
- **클라이언트 키**: filegate가 생성을 통제하지 않는 고엔트로피 랜덤(`fg_` 접두사 권장). 등록·저장은 sha256 해시만(`sha256:<64hex>`) — raw는 서버에 도달하지 않는다. 회전 = 해시 행 추가·삭제. raw의 배달은 생성자(Terraform)가 대상 서비스의 기존 시크릿 경로로 한다.
- **provider 시크릿**: 등록 요청에 원문이 담긴다 → 즉석 접근 검증 → AES-256-GCM으로 암호화 저장 (AAD에 provider id 바인딩, 마스터 키는 `FILEGATE_ENC_ROOT_SECRET` env). filegate가 서명에 원문을 써야 하므로 해시가 아니라 암호화다 — opsgate의 credential 보관 방식. 벤더 키 로테이션 = 벤더에서 새 키 발급 → 등록 갱신 (재시작 없음). 마스터 키 회전은 `enc_key_id` 컬럼으로 대비한다.
- **시크릿의 출생지와 배달**: Terraform이 발급·생성(state) 하고 k8s Secret(filegate의 마스터 키·토큰·DB URL)과 등록 API(provider 시크릿)로 배달한다. TF state 백엔드 보호가 전체 비밀 체계의 전제다.

## v0 배치: 명시 선언만

- profile은 provider **하나**를 가리킨다. create는 intent → profile → provider를 해석해 그곳에만 저장한다.
- 자동 선택 없음, 자동 이동 없음. 선언을 바꾸면 새 파일만 새 곳으로 간다. 후보 풀·선택 전략은 자동 배치가 올 때 확장한다 (ADR 001의 방향).

## 자동화 단계 (방향)

- **Level 0 (v0)**: 이동 없음.
- **Level 1**: reconciler가 이동 계획만 계산해 기록하고, 운영자 승인 후에만 집행한다 (plan/approve).
- **Level 2**: 명시적으로 켠 profile만 승인 없이 자동 수렴한다. 기본은 manual이다.

## 운영자 제어

- **운영자 API가 유일한 제어점이다.** 등록 CRUD와 운영 동사(usage, 이후 plan/approve/pause)가 여기 산다. 인증은 정적 운영자 토큰(env).
- 클라이언트: **Terraform provider**(선언 관리 — Go 위성 프로젝트, 같은 API의 번역기)와 **MCP**(AI — opsgate 경유)로 시작한다. CLI는 필요해지면 같은 API의 클라이언트로 추가한다. 화면은 두지 않으며, 생기더라도 이 API의 얇은 클라이언트다.
- API는 클라이언트-친화 CRUD로 만든다: 안정 id, id 단건 조회, 명확한 404, 멱등 삭제 — Terraform의 Read/plan이 요구하는 성질이다.

## 경계선

- 이 문서는 모양과 방향만 정한다. 스키마·엔드포인트·CLI 동사 목록은 구현의 영역이다.
- 구축 순서: 등록 테이블 → provider 등록 API(시크릿 암호화 때문에 SQL 시드 불가; profiles·clients는 SQL 시드 가능) → 도메인 오퍼레이션([spec 00](00-operations.md)) → 나머지 운영자 API → Terraform provider.
- 클라이언트 인증 미들웨어(키 해시 → client 신원 부착)는 인증이 필요한 첫 오퍼레이션과 함께 구현한다. filegate 자체 키다 — authgate에 의존하지 않는다 (공리 3).
