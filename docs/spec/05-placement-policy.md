# spec 05: 배치 정책

- Status: Accepted
- Date: 2026-07-21
- 근거: ADR [007](../adr/007-tiering-policy.md) (배치는 storage 소유 정책으로 cold에 수렴), [004](../adr/004-config-layers.md) (정본은 DB, 선언은 Terraform), [002](../adr/002-lease-model.md) (idle은 lease_history 관찰)
- 위에 얹힘: [spec 04](04-moves.md) (정책이 생성하는 이동을 집행하는 프리미티브)

어떤 파일을 언제 다른 storage로 옮길지 자동 결정하는 배치 정책의 계약을
정한다. 정책은 **이동을 생성만** 하고 집행은 spec 04의 이동 메커니즘이
한다 — 생성기와 집행기는 완전히 분리된다. 그래서 정책이 아무리 틀려도
최악은 "불필요한 이동"이지 손실이 아니다 (이동의 황금률이 보장).

## 정책 — source storage가 소유하는 규칙

정책은 **출발지 storage에 붙는 등록부 리소스**다 (credential이 client에
붙는 것과 같은 관계, spec 01). "이 storage에서 조건을 만족하는 파일은
목적지로 떠나야 한다"는 선언이다. 정본은 DB, 선언은 Terraform
`filegate_storage_policy`와 운영자 API다 (ADR 004).

- **모양**: `(우선순위, 조건들, 목적지)`. 스키마·필드 목록은 구현의
  영역이다 — 이 문서는 모양과 방향만 정한다 (spec 01과 같은 경계).
- **우선순위**: 낮을수록 먼저. 운영자 수동 이동(0, spec 04)을 정책이
  추월하지 못한다. 정책 간 서열도 이 값이다.
- **목적지**: 이동이 향할 storage. 정책은 출발지·목적지 쌍일 뿐이라
  강등과 승격이 같은 문법이다 (방향 중립, ADR 007).

## 조건 — nullable 필드의 AND

조건은 있는 것만 적용된다 (없으면 무시). 새 조건은 파서가 아니라 필드
하나를 additive로 더한다 (DSL 없음, ADR 007 기각).

| 조건 | 뜻 |
|---|---|
| `min_size` | 이 크기 이상이면 (대용량의 cold 직행) |
| `min_idle` | 마지막 read가 이보다 오래됐으면 (강등) |
| `max_idle` | 마지막 read가 이보다 최근이면 (승격 — 후속) |
| `high_pct` / `low_pct` | 압력 트리거 — 사용량이 high를 넘을 때만 작동, low에서 멈춤 (히스테리시스). 없으면 무조건 |

- **idle의 정의**: 그 파일의 마지막 read lease 시각, 없으면 확정 시각.
  원장(lease_history)의 관찰이며 별도 카운터가 없다 (ADR 002).

## 평가 — 생성기 루프

reconciler가 매 tick 실행한다. 바이트·벤더 호출 없이 이동 저널에
INSERT만 한다 (결정·집행 분리).

- **압력 게이트**: `high_pct`가 있으면 `사용량 > high × capacity`일 때만
  작동하고 `low`에서 멈춘다. 없으면 조건 매칭 즉시 (무조건).
- **우선순위 순, 첫 매칭 승리**: 정책을 우선순위 순으로 순차 평가한다.
  앞 정책이 집은 파일은 빠지고 나머지가 다음 정책의 입력이다 — 필터
  체인이 결과적으로 cold로 수렴한다.
- **coldest 우선**: 후보를 idle(마지막 read ASC, 크기 보조)로 정렬해
  위에서부터 집는다 — 가장 식은 것이 먼저 내려간다.
- 매칭 파일마다 목적지로 향하는 `object_moves` 행을 INSERT한다. 이후는
  전부 spec 04다 (복사→검증→스왑→지연삭제).

## 가드 — 정책은 배수구지 집행자가 아니다

- **생성만** — 바이트 안전은 이동 메커니즘이 보증한다 (spec 04). 정책층은
  데이터를 잃을 수 없다.
- **핑퐁 방지**: 최근 이동 제외(move_history 기준 쿨다운) + high/low
  히스테리시스. 양방향 정책이 같은 파일을 오르내리지 않는다.
- **유계**: tick당 이동 생성 상한(벤더 요청 예산 보호) · 진행 중 이동
  제외 · 목적지 여유 확인.
- **이동 가드 상속**: active·동종 kind·복사 상한(단일 PUT 5GiB)·같은
  물리 타깃 거부 (spec 04). 정책이 뽑아도 이동이 거부하면 no-op다.
- **capacity는 집행하지 않는다**: 업로드는 초과해도 받고, 정책이 빠르게
  빼낸다 — 과금이 용량-시간이라 짧은 초과는 비용이 없다 (ADR 007).

## 운영자 표면

정책은 storage 하위 리소스다 — 등록부의 CRUD와 같은 결이다 (spec 01).

| 동사 | 호출 | 뜻 |
|---|---|---|
| 정책 등록 | `POST /api/admin/v1/storages/{id}/policies` `{priority, destination_storage_id, conditions}` | 201 |
| 정책 목록 | `GET /api/admin/v1/storages/{id}/policies` | 우선순위 순 |
| 정책 조회·수정·삭제 | `GET·PUT·DELETE /api/admin/v1/storages/{id}/policies/{policy_id}` | |
| 자가점검 | `filegate status` | 정책별 `last_run · 생성 수 · last_error` 요약 |

- 관찰: 정책 행에 `last_run_at`·`last_error`·생성한 이동 수를 남긴다.
  생성된 이동의 진행은 `/moves`가, 종결은 `/moves/history`가 보인다
  (spec 04) — 세 층이 status와 API로 보인다.

## 이번 범위

- **압력 강등**: `high_pct`/`low_pct` + `min_idle`로 hot storage가 무료
  구간을 넘으면 coldest부터 동종 kind의 warm으로 내린다 (R2→OCI).
  정책 리소스 + 평가 엔진 + 생성 경로.

## 경계선

- **승격**(`max_idle`)은 새 기능이 아니라 정책 한 줄이다 — 도입 시점은
  운영 관찰이 정한다.
- **NAS(cold) 합류**는 후속이다 — cross-kind(s3↔fs) 이동과 5GiB 초과
  multipart 복사가 선행한다 (spec 04의 후속). 제외된 파일은 관찰
  가능해야 한다.
- 이 문서는 모양과 방향만 정한다. 정책 스키마·엔드포인트 상세는 구현의
  영역이다.
