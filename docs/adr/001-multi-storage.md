# ADR 001: storage는 코드가 아니라 데이터다

- Status: Draft
- Date: 2026-07-03 (개정 2026-07-10: 용어 provider→storage, profile 제거)
- 부모: [000](000-identity.md) 공리 1 (물리는 filegate가 소유한다)

## 문제

스토리지 벤더마다 기능, 주소, 인증 방식이 다르고 벤더 상태도 변한다. 벤더를 코드에 고정하면 filegate가 그 변화에 묶인다. 파일 배치 지식은 filegate가 갖는다.

## 결정

- **storage는 접근 계약이다.** 벤더, 계정, 주소, 자격증명, 공간을 묶는다. 같은 벤더의 다른 계정도 다른 storage다.
- **S3 호환 API를 1차 계약으로 삼는다.** S3, R2, OCI, MinIO, Garage를 같은 adapter로 다룬다. 벤더 native adapter는 필요가 확인될 때만 추가한다.
- **파일시스템도 storage다.** NFS 마운트 같은 로컬 경로는 fs adapter로 다룬다. presigned 개념이 없으므로 항상 중계 모드다.
- **내부 주소와 공개 주소를 구분한다.** 서명 URL은 주소에 묶인다. filegate가 쓰는 주소와 전송 주체가 쓰는 주소는 다를 수 있다.
- **capability는 선언식이다.** 런타임 탐지 대신 운영자가 선언하고, 틀리면 해당 storage 사용 시 실패한다. 선언 단위는 오퍼레이션이다 — 같은 storage가 읽기는 직결, 브라우저 쓰기는 중계일 수 있다.
- **file과 location을 분리한다.** file은 정체성이고, location은 바뀔 수 있는 위치다.
- **배치는 등록이 정한다.** v0는 binding이 가리키는 storage 하나에 저장한다 — 명시 선언만 ([spec 01](../spec/01-registry.md)). 후보 풀과 선택 전략은 자동 배치가 올 때 확장한다.

## 경계선

- 자동 비용 최적화와 이동은 reconciler가 요청 경로 밖에서 처리한다.
- 객체 이름은 filegate가 발급한 불투명 키다 (클라이언트 파일명과 무관).
- 비용과 무료 구간은 [docs/vendors/](../vendors/README.md)에 둔다.

## 결과

- 벤더나 계정 추가는 등록 하나로 처리한다. S3 호환이면 코드 변경은 없다.
- 로컬의 MinIO와 이후의 OCI가 같은 adapter, 같은 코드 경로.
- 깨진 참조는 쓰기 시점에 거부된다(FK). 자격증명 누락·공간 접근 불가는 등록 거부로, 이후 변질은 부팅 재검증 실패로 드러난다.
