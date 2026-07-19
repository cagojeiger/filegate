# 서비스 연동 가이드

- Date: 2026-07-13
- 근거: [spec 00](../spec/00-operations.md) (오퍼레이션 계약), ADR [003](../adr/003-url-ownership.md)·[005](../adr/005-presigned-byte-plane.md)

서비스가 filegate를 파일 저장 계층으로 붙이는 방법을 정한다. 서비스는
file_id만 알고, 물리(벤더·경로·수명)는 filegate가 소유한다.

> **인터페이스 계약** (ADR 005): 바이트 인터페이스는 서명·만료 URL의
> 발급이다 — create(업로드), read(다운로드), parts(multipart part).
> 이것이 전부다.

## 두 평면

제어와 바이트는 다른 길을 간다. 서비스는 제어(작은 JSON)만 다루고,
바이트는 전송 주체(브라우저 또는 서비스 서버)가 URL의 목적지와 직접
주고받는다.

```
             ┌──── 제어(JSON) ────┐
브라우저 ⇄ 서비스 ⇄ filegate ─(서명만)─→ 저장소
   │                                        ▲
   └────────── 바이트(직결 s3) ─────────────┘
        fs/NFS는 filegate 서명 URL로 filegate를 경유해 닿는다 — 계약은 동일
```

## 등록 (1회, Terraform)

storage(벤더 자격증명)와 client(서비스 신원 + 키)를 등록한다 — client는
자기 기반 storage(`storage_id`)를 직접 소유한다 ([spec 01](../spec/01-registry.md)).
이후 서비스는 클라이언트 키 하나로 인증한다.

## 업로드

```
① POST /api/v1/files {declared_size, content_type}
     → { file_id, put_url }                        · 파일은 pending으로 예약된다
② 전송 주체가 put_url로 바이트 PUT                  · 브라우저 위임 또는 서버 직접
③ POST /api/v1/files/{id}/commit
     → { state: "active", etag }                   · filegate가 실물 크기를 대조해 승격
④ state == "active"면 file_id를 서비스 DB에 저장    · 이것이 업로드 완료의 정의
```

완료의 판정자는 commit이다. 전송 주체의 PUT 성공(200)은 commit을 부르는
신호로만 쓴다 — filegate는 저장소 실물을 직접 확인하고 승격한다.

대용량(임계값 초과)이면 ①의 응답이 put_url 대신 `{part_size, part_count}`
서술자를 준다. 전송 주체는 `POST /api/v1/files/{id}/parts {parts:[...]}`로
part별 URL을 받아 나눠 올리고, 끊기면 그 part만 다시 받는다(재개).
commit은 동일하다.

## 업로드 실패

commit되지 않은 파일은 lease 만료(15분) 후 filegate가 저장소 실물까지
자동 회수한다. 서비스의 실패 처리 = 성공 경로만 처리하는 것이다.
재시도는 15분 내 같은 URL로 다시 PUT하고 commit한다.

## 다운로드 · 조회 · 삭제

| 동사 | 호출 | 반환 |
|---|---|---|
| 다운로드 | `POST /api/v1/files/{id}/read` (선택: filename) | `{ get_url }` — 15분 만료, 전송 주체가 직접 GET |
| 조회 | `GET /api/v1/files/{id}` | `{ state, declared_size }` |
| 삭제 | `DELETE /api/v1/files/{id}` | 삭제 결정 기록 — 물리 정리는 filegate 몫 |

read는 매번 현재 위치를 해석해 새 URL을 만든다 — 파일이 이동해도 같은
file_id가 동작한다. URL은 일회용이다: 서비스는 file_id만 저장하고, URL은
그때그때 받아 전달한다 (ADR 003).

## 두 개의 장부

의미는 서비스가, 물리는 filegate가 소유한다. file_id 하나가 둘을 잇는다.

```
서비스 DB:   누구의 무엇인지 — 소유자, 맥락, 파일명, 권한, 목록·검색
filegate:    어디에 얼마나 — 위치, 크기, 상태, 수명, 사용량 관찰
```

서비스 스키마 예: `attachments(id, note_id, filegate_file_id, filename, ...)`

삭제는 두 장부를 함께 정리한다: `DELETE /api/v1/files/{id}` + 서비스 행 삭제.

## 인증

| 표면 | 인증 |
|---|---|
| `/api/v1/*` | 클라이언트 키 (`Authorization: Bearer <key>`) |
| 바이트 URL | URL 자체가 자격 (서명·만료) — 추가 인증 없음 |
| `/api/admin/v1/*` | 운영자 토큰 — 등록·상태·사용량 관찰은 운영자의 세계 |

## 체크리스트

- [ ] Terraform: storage + client(+ storage_id) + client_key
- [ ] 업로드: 권한검사 → create → PUT 위임 → commit → `active`면 file_id 저장
- [ ] 다운로드: read → get_url 전달
- [ ] 삭제: filegate delete + 서비스 행 삭제 (두 장부)
- [ ] 서비스가 영속화하는 filegate 산출물은 file_id 하나다
