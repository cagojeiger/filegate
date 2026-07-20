# spec 04: 이동과 배치 정책

- Status: Accepted
- Date: 2026-07-20
- 근거: ADR [007](../adr/007-tiering-policy.md) (배치는 storage 소유 정책으로 cold에 수렴), [001](../adr/001-multi-storage.md) (file/location 분리), [002](../adr/002-lease-model.md) (관찰 원장)

파일 하나의 storage를 바꾸는 이동 오퍼레이션과, 이동을 생성하는 배치
정책의 계약을 정한다. 정책 부분은 모양과 방향을 정한다 — 스키마·필드
목록은 구현의 영역이다 ([spec 01](01-registry.md)과 같은 경계).

## 이동 — 한 파일의 안전한 재배치

순서가 계약이다. 각 단계가 실패해도 앞 단계의 상태는 유효하다.

| 단계 | 무엇 | 불변식 |
|---|---|---|
| ① 복사 | source 실물을 **같은 object_key**로 dest에 쓴다 | 키가 같아 재시도가 멱등이다 |
| ② 검증 | 크기 실측 대조 + 기록 etag가 평문 md5면 대조 | 통과 전 스왑 없음 |
| ③ 스왑 | location 포인터를 조건부 전이(한 tx)로 dest로 | 0행이면 롤백 — 경합 패배 |
| ④ 지연 삭제 | presigned 수명(기본 900초) 뒤에만 source 실물 제거 | 발급된 읽기 URL이 죽지 않는다 |

- **경합 규칙**: 이동 중 삭제·덮어쓰기·취소는 요청 경로가 이기고
  이동이 조용히 진다. 진 이동의 dest 잔여물은 reconciler가 치운다.
- **읽기와의 공존**: 읽기는 location을 매 요청 재해석한다 — 스왑
  전엔 source, 후엔 dest를 보고 둘 다 실물이 있다. 스왑 이전에 발급된
  URL은 ④의 지연이 덮는다.

## 이동 저널 — 진행의 유일한 상태

```
requested ──집행──▶ swapped ──지연삭제──▶ (행 삭제 = 완료)
    │ 취소               │
    ▼                    ▼ (경합 패배·정리)
 canceled ──정리──▶ (행 삭제)
    │
 failed  ← 재시도 소진 (park — 운영자 재요청이 재무장)
```

- 파일당 진행 중 이동은 최대 하나다 (저널 PK).
- 재시도는 정책이다: `FILEGATE_MOVE_MAX_ATTEMPTS`(기본 5) ·
  `FILEGATE_MOVE_RETRY_BACKOFF_SECS`(60) ·
  `FILEGATE_MOVE_DELETE_DELAY_SECS`(900, 1 미만 거부).
- **집행 우선순위**: 낮을수록 먼저. 운영자 수동(0)이 정책(기본 100)을
  항상 추월한다. 정책 간 서열도 이 값이다.
- 저널의 storage FK가 이동 중 storage 삭제를 거부한다 (spec 01 삭제
  방패의 연장).

## 이동 원장 — 종결의 박제

종결(moved·lost·canceled)마다 reconciler가 저널 행 삭제와 **같은 tx**로
원장에 기록한다. 원장은 FK 없이 client·크기를 박제해 파일·storage 행이
정리된 뒤에도 홀로 읽힌다. 보존·정리는 lease 원장과 같은 결이다.

## 운영자 표면

이동은 비동기 job이다 — 요청은 저널에 requested를 남기고 집행은 reconciler가
한다. 그래서 이동을 1급 job 리소스(`/moves`)로 모델링한다: 요청은 생성이고,
진행은 폴링이며, fleet은 컬렉션이다 (Stripe의 Refund·Google LRO와 같은 결).
`/files`는 이동 대상 선정을 위한 인벤토리다 — 물리는 운영자만 본다.

| 동사 | 호출 | 뜻 |
|---|---|---|
| 이동 요청 | `POST /api/admin/v1/moves` `{file_id, storage_id}` | job 생성 → 202. active·동종 kind·≤5GiB만. 진행 중 이동 있으면 409 |
| 이동 조회 | `GET /api/admin/v1/moves`·`/moves/{file_id}` | 진행 중 job (저널). 없으면 404 |
| 이동 취소 | `DELETE /api/admin/v1/moves/{file_id}` | canceled 표시만 — 정리는 reconciler. swapped는 409(늦음) |
| 이력 조회 | `GET /api/admin/v1/moves/history` | 종결 원장 |
| 파일 목록 | `GET /api/admin/v1/files?storage_id=&state=&limit=&after=` | 인벤토리 (keyset 페이지네이션) — idle 신호 포함 |
| 파일 상세 | `GET /api/admin/v1/files/{id}` | location·진행 중 이동 포함 |
| 자가점검 | `filegate status` | `MOVES active n · failed m` 요약 — failed가 있으면 exit 1 |

## 배치 정책 — 이동의 생성기

정책은 storage가 소유하는 등록부 리소스다 (credential이 client에
붙는 것과 같은 관계). Terraform `filegate_storage_policy`와 운영자
API(`/api/admin/v1/storages/{id}/policies`)로 선언한다.

- **모양**: `(우선순위, 조건들, 목적지)`. 조건은 nullable 필드의
  AND다 — 있는 것만 적용된다:

| 조건 | 뜻 |
|---|---|
| `min_size` | 이 크기 이상이면 (대용량의 cold 직행) |
| `min_idle` | 마지막 read가 이보다 오래됐으면 (강등) |
| `max_idle` | 마지막 read가 이보다 최근이면 (승격) |
| `high_pct` / `low_pct` | 압력 트리거 — 사용량이 high를 넘을 때만 작동, low에서 멈춤. 없으면 무조건 |

- **평가**: reconciler가 tick마다 우선순위 순으로 순차 평가한다. 앞
  정책이 집은 파일은 뒤 정책의 입력에서 빠진다 (첫 매칭 승리). 조건
  미달이면 no-op — 평가는 값싸다.
- **idle의 정의**: 그 파일의 마지막 read lease 시각, 없으면 확정
  시각. 원장(lease_history)의 관찰이며 별도 카운터가 없다.
- **가드**: tick당 이동량 상한(벤더 요청 예산 보호) · 최근 이동
  제외(이동 원장 기준 쿨다운 — 양방향 정책의 핑퐁을 끊는다) · 진행 중
  이동 제외 · 복사 상한(단일 PUT 5GiB) 초과 파일 제외.
- **실패 기록**: 정책 평가 실패는 정책 행(last_run·last_error)에
  남고 다음 정책은 계속 평가된다. 개별 이동 실패는 저널이, 종결은
  원장이 기록한다 — 세 층이 status와 운영자 API로 보인다.

## 경계선

- 이 문서는 모양과 방향만 정한다. 정책 스키마·엔드포인트 상세는
  구현의 영역이다.
- 승격은 새 기능이 아니라 `max_idle` 정책 한 줄이다 — 도입 시점은
  운영 관찰이 정한다.
- 다른 kind로의 이동(s3↔fs)과 5GiB 초과(multipart 복사)는 후속이다.
  제외된 파일은 관찰 가능해야 한다.
