# ADR 004: intent는 서비스의 어휘고, 배치는 운영자의 카탈로그다

- Status: Draft
- Date: 2026-07-03
- 부모: [000](000-identity.md) 공리 1 (소유 분리), 공리 3 (내부 의존 0)

## 문제

filegate는 여러 서비스가 쓴다. intent를 전역으로 두면 이름 충돌과 소유권 문제가 생긴다. 용량에는 quota와 capacity가 있다. 둘은 따로 계산해야 한다.

## 결정

- **설정은 소유자가 다른 세 계층이다.**
  ```text
  providers          물리 사실   (접근 계약의 목록)
  storage_profiles   운영자 결정 (배치·수명 정책 카탈로그)
  clients            서비스 어휘 (자격증명 + quota + 자기 intents)
  ```
- **intent는 클라이언트 블록 안에 둔다.** 클라이언트별 네임스페이스를 가진다.
- **intent는 profile을 가리킨다.** provider를 직접 가리키지 않는다. 배치를 바꿔도 서비스 계약은 유지된다.
- **회계는 quota와 capacity를 나눈다.** 쓰기 발급 시 둘 다 예약한다. 확정 시 정산하고, 만료·취소 시 해제한다. 한쪽이라도 초과하면 발급하지 않는다.
- **capacity 기본값은 무료 구간이다.** 값은 [vendors/](../vendors/README.md)를 기준으로 한다. capacity가 부족해지면 tiering의 입력이 된다.

## 경계선

- filegate는 intent 이름의 의미를 해석하지 않는다.
- 오버커밋(quota 합 > capacity)은 허용하되 부팅 시 표시한다.
- capacity가 부족하면 quota가 남아도 발급하지 않는다.
- 깨진 참조(없는 profile, 없는 provider)는 부팅 실패.
- 관리 화면은 두지 않는다. 세 계층 모두 배포 설정을 기준으로 한다.

## 결과

- 새 서비스 온보딩은 clients 블록 하나로 처리한다.
- 클라이언트는 다른 클라이언트의 영역을 수정할 수 없다.
- 사용량 조회는 두 시점: 클라이언트는 자기 몫의 소진, 운영자는 공간별 합산.
- 초기 구현(전역 intent 풀, 단일 축 usage_counters)은 이 구조로 재편된다.
