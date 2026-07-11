# spec 02: multipart 오퍼레이션

- Status: Draft
- Date: 2026-07-11
- 근거: ADR [002](../adr/002-lease-model.md) (multipart는 lease의 확장, 검증 단위는 part), [001](../adr/001-multi-storage.md) (모드는 storage 선언이 결정)
- 참고: MinIO 소스 분석 (2026-07-11) — 세션은 durable 객체, part는 불변
  temp→rename, complete는 조립이 아니라 manifest 커밋, ETag는 digest-of-digests.

단일 PUT 상한을 넘는 파일, lease TTL보다 오래 걸리는 전송, part 단위 재시도를
정한다. spec 00의 계약(2단계 커밋, 상태기계, 회계)은 변하지 않는다 — 이 문서는
쓰기 접근의 모양과 검증 단위만 더한다.

## 범위

이번 범위: multipart `create` 분기, `parts`(part 접근 발급·재발급), multipart
`commit`, 회수 확장(벤더 multipart 중단).

다음 범위로 미룬다: part 내부 오프셋 재개(tus류 — part 크기가 재전송 상한),
full-object CRC 합성 검증(part CRC의 수학적 합성 — 선택 보강), 병렬 업로드
가속 힌트.

## 결정 사항

### 발급 — create의 크기 분기

- `declared_size > multipart 임계값`(운영자 설정)이면 create는 PUT URL 대신
  **multipart 서술자**를 돌려준다: `{file_id, multipart: {part_size, part_count}}`.
- part 크기는 filegate가 정한다 (운영자 설정, 균일 — 마지막 part만 나머지).
  R2의 균일 크기 요구를 자동 충족하고, 중계 fs의 offset 계산 전제다.
- **part_size는 업로드별로 동결한다** — create 시점 설정값을 파일 행에
  고정 저장. 운영자가 설정을 바꿔도 진행 중 업로드의 offset 파생이
  흔들리지 않는다. part 기하(개수·offset·명목 크기)는 이 동결값 +
  declared_size에서 전부 파생하며 **저장하지 않는다** — DB에 남는 part
  정보는 실측(크기·체크섬)과 승격 상태뿐이다.
- 직결 s3의 벤더 세션 핸들(upload_id)은 write lease에 저장한다 — 외부
  핸들이라 파생 불가능한 유이(唯二)한 저장 항목이다 (동결 part_size와 함께).
- 임계값 이하는 spec 00 그대로 — 단일 PUT 경로는 계약도 코드도 변하지 않는다.
- multipart create는 `declared_md5`를 받지 않는다 (400). multipart의 검증
  단위는 part다 (ADR 002) — 전체 MD5는 어떤 모드에서도 실측되지 않는 값이라
  선언을 받는 것 자체가 거짓 계약이다.
- 회계는 동일: declared_size 전체를 create가 예약한다. part는 회계 단위가
  아니다.

### parts — part 접근 발급 = 갱신 = 재개

- 입력: file_id + part 번호 목록. 출력: part별 만료 있는 URL.
- 같은 part의 재요청이 곧 재개다. 발급마다 write lease의 만료가 연장된다 —
  진행 중인 multipart는 발급이 이어지는 한 회수되지 않는다. lease TTL을
  넘기는 전송 문제가 여기서 해소된다 (ADR 002의 갱신).
- URL의 정체는 모드가 정한다 (서비스는 모른다): 직결이면 벤더 part presigned
  URL, 중계면 `/b/{lease}?s=…&part=N`.
- part 재업로드는 last-write-wins다 (S3 의미론과 동일).

### 전송 — 모드별 물리

서비스 계약은 같다. 아래는 filegate 내부의 확정 사항이다.

- **직결 s3**: create 시 벤더 multipart 세션을 시작하고 upload_id를 lease에
  기록한다. parts는 벤더 part presigned URL을 발급한다. 바이트는 filegate를
  지나지 않는다 (공리 2).
- **중계 fs**: part는 요청별 고유 임시 파일로 받고(계측 포함), part 행의
  조건부 claim으로 승격을 직렬화한 뒤 대상 임시 파일의 자기 offset
  (`(N-1) × part_size`)에 기록한다. 범위가 겹치지 않아 병렬·멀티 pod에
  안전하다. 같은 part 동시 PUT의 인터리브 손상은 claim이 막는다 —
  단일 PUT의 temp 충돌과 같은 병을 part 단위에서 같은 처방으로 막는 것.
- **중계 s3**: part를 스풀로 계측해 받고 도착 즉시 벤더 part로 올린 뒤
  스풀을 지운다. 디스크 점유는 진행 중 part 수에 유계다.
- 중계 part의 계측은 단일 PUT과 같다: Content-Length 필수, part 크기와
  일치, 초과 시 차단, 유휴 타임아웃, 크기·체크섬 실측 기록.

### commit — 입력은 여전히 file_id 하나다

- 서비스는 part 목록을 제출하지 않는다. filegate가 자기 원장(중계: part
  실측 기록)과 벤더 실물(직결: ListParts)을 대조해 완성한다 — part의 진실
  원천은 클라이언트가 아니라 filegate다. (MinIO/S3는 클라이언트 리스트를
  받지만, 그건 게이트웨이가 없는 프로토콜의 사정이다. 우리 서비스 계약은
  spec 00의 commit과 동일하게 유지한다.)
- 검증: 모든 part가 존재하고, part 크기 합 = declared_size, part별
  실측(중계) 또는 벤더 ETag(직결)가 기록과 일치.
- 확정: 직결·중계 s3는 벤더 complete, 중계 fs는 rename 한 번 (조립 없음 —
  offset 기록이 이미 조립이다).
- 기록되는 ETag는 벤더 multipart ETag(digest-of-digests, `-N` 접미) 또는
  중계 fs의 part 체크섬 합성값이다. 단일 PUT처럼 전체 MD5가 아니다.
- 미완성(빠진 part, 크기 불일치)이면 400과 함께 pending에 남는다 —
  spec 00의 재시도 계약 그대로.

### 회수 — reconciler 확장

- pending 만료 회수는 벤더 multipart **중단(Abort)**을 포함한다 — s3는
  중단하지 않은 미완성 part에 과금된다.
- 중계 fs는 대상 임시 파일과 part 스풀을 지운다. part 행은 lease에 따라
  정리된다.
- 나머지(전이 우선, 정산, 멱등)는 spec 00의 회수와 같다.

## 상태

파일 상태기계는 spec 00 그대로다. 더해지는 것은 lease 아래의 part 기록뿐이다:

```text
part: (미기록) ──중계 PUT 완료 / 직결은 commit의 ListParts──▶ 실측 기록
lease: 발급 ──parts 재발급(만료 연장)*──▶ commit으로 확정 | 만료 회수(+Abort)
```

## 경계선

- 파일 크기 상한 = `min(part_size × 10000, 벤더 한계, 운영자 설정)`.
  단일 PUT의 5GiB 상한은 그대로 남는다 (multipart 임계값과 별개).
- part 번호·크기는 응답에 담기는 사실이지 계약이 아니다 — 서비스는 받은
  서술자대로 자르고, 구조에 의존하지 않는다 (ADR 003과 같은 원칙).
- multipart 임계값·part 크기·상한은 운영자 설정이다. 기본값은 구현이 정하고
  .env.example에 기록한다.
- 진행 중 multipart의 part는 파일이 아니다 — stat은 여전히 pending이고,
  read는 여전히 409다. commit 전 바이트는 파일이 아니라는 문장이 part에도
  적용된다 (ADR 002).
- 완료 조건: 같은 대용량 시나리오(경계 크기 포함)가 직결과 중계에서 같은
  상태 전이·회계·응답을 내는 동등성 E2E.
