# spec 04: 이동

- Status: Accepted
- Date: 2026-07-20
- 근거: ADR [001](../adr/001-multi-storage.md) (file/location 분리 — 이동은 포인터 교체), [002](../adr/002-lease-model.md) (관찰 원장)

파일 하나의 storage를 바꾸는 이동 오퍼레이션의 계약을 정한다 — 운영자가
수동으로 트리거하고 reconciler가 집행한다. 어떤 파일을 언제 옮길지 자동
결정하는 배치 정책은 다음 범위다. 이 문서는 그 정책이 집행에 쓸
프리미티브(안전한 재배치 + 운영자 표면)를 정한다.

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
- **집행 우선순위**: 낮을수록 먼저. 운영자 수동은 0이다 — 배치 정책층
  (다음 범위)이 이 값으로 서열을 매기고 수동을 추월당하지 않게 둔다.
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

## 경계선

- **배치 정책은 다음 범위다** — 어떤 파일을 언제 옮길지 자동 결정하는
  정책층(storage 소유, 조건 필터, 스케줄러가 이동을 생성)은 이
  프리미티브 위에 얹힌다. 이번 범위는 프리미티브와 수동 트리거뿐이다.
- 다른 kind로의 이동(s3↔fs)과 5GiB 초과(multipart 복사)는 후속이다.
  제외된 파일은 관찰 가능해야 한다.
