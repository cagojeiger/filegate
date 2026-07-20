# ADR

ADR은 방향, 구조, 원칙만 담는다. 범위별 오퍼레이션 계약은 [docs/spec/](../spec/00-operations.md)에, 가격처럼 변하는 외부 사실은 [docs/vendors/](../vendors/README.md)에, 언어·프레임워크·크레이트 같은 기술 선택은 [docs/stack/](../stack/README.md)에 둔다. 구현 세부사항은 코드에 둔다.

## 용어

| 용어 | 뜻 |
|---|---|
| **등록부 (registry)** | storages·clients·자격증명·정책의 정본. DB에 살고 운영자 API로만 변경한다 |
| **클라이언트 (client)** | filegate에서 lease를 받는 서비스의 등록 단위. 등록 하나가 서비스 하나다 |
| **전송 주체** | 바이트를 실제로 옮기는 쪽. 사용자의 브라우저 또는 서비스 서버 |
| **스토리지 (storage)** | 저장 공간 접근 계약. 벤더, 계정, 주소, 자격증명, 공간을 묶는다. 시크릿은 암호화되어 등록부에 산다 |
| **file / location** | 파일의 정체성 / 물리 위치. location은 바뀔 수 있다 |
| **이동 (move)** | location의 storage 교체. 복사→검증→스왑→지연삭제로 집행된다 ([spec 04](../spec/04-moves.md)) |
| **정책 (policy)** | storage가 소유한 배치 규칙 — 조건을 만족하는 파일의 이동을 생성한다. 등록부에 산다 ([ADR 007](007-tiering-policy.md)) |
| **lease** | 시간제한·단일목적 접근 권한. 모든 바이트 접근의 단위다. 취소는 원장 기준이다 — 발급된 직결 URL은 만료로만 소멸한다 |
| **직결 / 중계** | 전송 주체가 바이트에 닿는 두 모드. 직결은 저장소가 서명한 URL, 중계는 filegate의 바이트 엔드포인트. storage capability가 결정한다 |
| **논리키 (logical key)** | S3 호환 표면에서 서비스가 정하는 object key. 서비스 소유 이름이며 (client, 논리키) → file 매핑으로 산다 ([ADR 006](006-s3-compat-surface.md)) |
| **quota** | 운영자가 클라이언트별로 정한 용량 몫. 운영자 내부 가드레일이며 클라이언트에게 노출되지 않는다 |
| **capacity** | storage 등록에 적힌 용량 기준선. 집행하지 않는다 — usage 관찰의 비교선일 뿐 ([spec 00](../spec/00-operations.md)) |
| **reconciler** | 요청 경로 밖에서 물리 상태를 정리하는 작업 |
| **detach / purge** | 삭제의 두 단계. detach는 서비스의 결정, purge는 reconciler의 물리 집행 |
| **tiering** | 파일이 정책 합성을 따라 cold storage로 수렴하는 재배치. 정책이 생성하고 reconciler가 집행한다 ([ADR 007](007-tiering-policy.md)) |

## 목록

| # | 제목 | 파생 |
|---|---|---|
| [000](000-identity.md) | 서비스는 파일의 물리를 관리하지 않는다 (세 공리) | — |
| [001](001-multi-storage.md) | storage는 코드가 아니라 데이터다 | 공리 1 |
| [002](002-lease-model.md) | 모든 바이트 접근은 lease다 | 공리 2 |
| [003](003-url-ownership.md) | 안정 URL은 서비스가 소유하고, filegate URL은 저장하지 않는다 | 공리 1+2 |
| [004](004-config-layers.md) | 어휘는 서비스, 카탈로그는 운영자, 정본은 DB다 (컨트롤 플레인) | 공유 전제 + 공리 1+3 |
| [005](005-presigned-byte-plane.md) | 네이티브 바이트 인터페이스는 서명 URL 발급 단 하나다 (실측 근거) | 공리 2 |
| [006](006-s3-compat-surface.md) | S3 호환 표면은 중계를 수용한 온보딩 계층이다 (무수정 SDK ↔ 양방향 중계) | 공리 2 + ADR 005 |
| [007](007-tiering-policy.md) | 파일 배치는 storage 소유 정책으로 cold에 수렴한다 | ADR 001+002+004 |
