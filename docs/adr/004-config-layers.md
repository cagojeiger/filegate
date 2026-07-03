# ADR 004: intent는 서비스의 어휘, 배치는 운영자의 카탈로그

- Status: Draft (최초 작성)
- Date: 2026-07-03
- 부모: [000](000-identity.md) 공리 1 (소유 분리) + 공리 3 (여러 서비스가 쓴다)

## 문제

filegate는 여러 서비스가 쓴다. intent를 전역 풀로 두면 이름 충돌과 소유권 불명이 자란다. 또 "용량"에는 두 진실이 섞여 있다 — client에게 약속한 몫(quota)과 공간의 물리 한도(capacity). 한 축의 회계로는 둘 중 하나를 거짓말하게 된다.

## 결정

- **설정은 소유자가 다른 세 계층이다.**
  ```text
  providers          물리 사실   (접근 계약의 목록)
  storage_profiles   운영자 결정 (배치·수명 정책 카탈로그)
  clients            서비스 어휘 (자격증명 + quota + 자기 intents)
  ```
- **intent는 client 블록 안에 산다.** 이름공간이 client별이라 충돌 개념이 없다. 서비스 추가 = 블록 하나 추가.
- **intent는 provider가 아니라 profile을 가리킨다.** 배치를 바꿔도 서비스 쪽 계약(intent 이름)은 흔들리지 않는다 — 공리 1의 설정판.
- **회계는 두 축.** 쓰기 발급 시 quota(약속)와 capacity(물리) 양쪽에 예약, 확정 시 정산, 만료·취소 시 해제. 한쪽만 초과해도 발급 거부. quota는 "이 서비스가 약속을 지키나", capacity는 "이 공간이 차오르나"에 답한다.
- **capacity의 기본값은 무료 구간** ([vendors/](../vendors/README.md) 참조). capacity 압박이 훗날 tiering(안 읽히는 것부터 밀어내기)의 트리거이며, 판단 근거인 접근 빈도는 lease 기록에 이미 있다(002).

## 경계선

- filegate는 intent 이름의 의미를 해석하지 않는다 — profile 포인터이자 회계 키일 뿐.
- 오버커밋(quota 합 > capacity)은 허용하되 부팅 시 가시화. 물리가 먼저 차면 quota가 남아도 거부 — 물리는 거짓말하지 않는다.
- 깨진 참조(없는 profile, 없는 provider)는 부팅 실패.
- 관리 화면 없음 — 세 계층 모두 배포 설정이 진실(000).

## 결과

- 새 서비스 온보딩 = clients 블록 하나. 남의 영역은 구조적으로 못 건드린다.
- 사용량 조회는 두 시점: client는 자기 몫의 소진, 운영자는 공간별 합산.
- 초기 구현(전역 intent 풀, 단일 축 usage_counters)은 이 구조로 재편된다.
