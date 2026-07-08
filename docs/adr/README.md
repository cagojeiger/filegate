# ADR

ADR은 방향, 구조, 원칙만 담는다. 범위별 오퍼레이션 계약은 [docs/spec/](../spec/00-operations.md)에, 가격처럼 변하는 외부 사실은 [docs/vendors/](../vendors/README.md)에 둔다. 구현 세부사항은 코드에 둔다.

## 용어

| 용어 | 뜻 |
|---|---|
| **클라이언트 (client)** | filegate에서 lease를 받는 서비스의 등록 단위. `clients` 블록 하나가 서비스 하나다 |
| **전송 주체** | 바이트를 실제로 옮기는 쪽. 사용자의 브라우저 또는 서비스 서버 |
| **프로바이더 (provider)** | 저장 공간 접근 계약. 벤더, 계정, 주소, 자격증명, 공간을 묶는다 |
| **intent** | 클라이언트가 쓰는 파일 용도 이름. 클라이언트별 네임스페이스를 가진다 |
| **profile** | 운영자가 정의한 배치·수명 정책 템플릿. intent가 참조한다 |
| **file / location** | 파일의 정체성 / 물리 위치. location은 바뀔 수 있다 |
| **lease** | 시간제한·단일목적 접근 권한. 모든 바이트 접근의 단위다. 취소는 원장 기준이다 — 발급된 직결 URL은 만료로만 소멸한다 |
| **직결 / 중계** | 전송 주체가 바이트에 닿는 두 모드. 직결은 저장소가 서명한 URL, 중계는 filegate의 바이트 엔드포인트. provider capability가 결정한다 |
| **quota** | 운영자가 클라이언트별로 정한 용량 몫. 운영자 내부 가드레일이며 클라이언트에게 노출되지 않는다 |
| **capacity** | provider의 물리 한도 (무료 구간, 디스크) |
| **reconciler** | 요청 경로 밖에서 물리 상태를 정리하는 작업 |
| **detach / purge** | 삭제의 두 단계. detach는 서비스의 결정, purge는 reconciler의 물리 집행 |
| **tiering** | capacity 압박 시 파일 location을 옮기는 재배치. reconciler가 집행한다 |

## 목록

| # | 제목 | 파생 |
|---|---|---|
| [000](000-identity.md) | 서비스는 파일의 물리를 관리하지 않는다 (세 공리) | — |
| [001](001-multi-provider.md) | provider는 코드가 아니라 설정이다 | 공리 1 |
| [002](002-lease-model.md) | 모든 바이트 접근은 lease다 | 공리 2 |
| [003](003-url-ownership.md) | 안정 URL은 서비스가 소유하고, filegate URL은 저장하지 않는다 | 공리 1+2 |
| [004](004-config-layers.md) | intent는 서비스의 어휘고, 배치는 운영자의 카탈로그다 | 공유 전제 + 공리 1+3 |
| [005](005-metadata-store.md) | 메타데이터 저장소는 PostgreSQL이다 | 공리 3 + 002 + 004 |
