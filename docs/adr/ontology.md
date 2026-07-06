# ADR 논리 그래프

ADR의 논리 구조를 두 그래프로 정리한다. 파생 그래프는 공리와 결정의 관계를, 개념 온톨로지는 용어 간 관계를 보여준다. 그래프와 ADR 본문이 다르면 ADR 본문을 기준으로 한다.

## 1. 파생 그래프: 공리 → 결정 → 경계

```mermaid
graph TD
    subgraph AX["000: 세 공리"]
        A1["공리 1<br/>소유 분리<br/>(의미=서비스, 물리=filegate)"]
        A2["공리 2<br/>바이트 직결이 기본<br/>(중계는 선언된 예외)"]
        A3["공리 3<br/>내부 의존 0"]
    end

    A1 --> B1["경계: 유저 개념 없음<br/>권한 검사는 서비스 몫"]
    A1 --> B2["경계: 클라이언트 메타데이터 불투명"]
    A1 --> B3["결정·집행 분리<br/>(detach는 서비스, purge는 filegate)"]

    A1 --> D001["001 provider는 설정이다"]
    D001 --> D001a["provider = 접근 계약<br/>(계정 단위 분리)"]
    D001 --> D001b["file / location 분리<br/>(위치는 가변 포인터)"]
    D001 --> D001c["capability 선언식"]
    D001b --> D001d["배치는 설정으로 선택<br/>(이동 가능)"]

    A2 --> D002["002 모든 바이트 접근은 lease다"]
    D002 --> D002a["commit = 검증 게이트"]
    D002 --> D002b["lease = 감사 단위"]
    D002 --> D002c["보안 = capability"]
    A2 -. "예외 절" .-> D002d["중계 모드<br/>(filegate 자체 발급, 서버기록형 토큰)"]
    A1 --> D002e["중계는 filegate에 둔다<br/>(서비스별 fallback 금지)"]
    D002d --- D002e

    A1 --> D003["003 안정 URL은 서비스의 것"]
    D002c --> D003
    D003 --> D003a["filegate에 익명 표면 없음<br/>(중계 바이트 엔드포인트만 예외)"]
    D003 --> D003b["filegate URL 저장 금지<br/>(임베드·캐시 금지)"]

    A1 --> D004["004 intent는 어휘, 배치는 카탈로그"]
    MT["미선언 전제:<br/>하나의 filegate를<br/>여러 서비스가 공유한다"] -.-> D004
    A3 -. "근거 보강 필요" .-> D004
    D004 --> D004a["설정 3계층<br/>providers / profiles / clients"]
    D004 --> D004b["회계 두 축<br/>quota(약속) + capacity(물리)"]

    style MT fill:#fff3cd,stroke:#cc8800,stroke-dasharray: 5 5
```

읽는 법: 실선은 문서에 근거가 있는 파생 관계다. 점선은 예외 또는 근거 보강이 필요한 관계다. 004의 "여러 서비스 공유" 전제는 아직 공리에 명시되어 있지 않다.

## 2. 개념 온톨로지: 용어와 관계

```mermaid
graph LR
    subgraph 의미세계["의미의 세계 (서비스 소유)"]
        USER["사용자"]
        SVC["서비스"]
        PERM["권한 판단"]
        SURL["안정 URL"]
    end

    subgraph 경계["경계 (설정이 정의)"]
        CL["클라이언트<br/>(서비스의 등록)"]
        IN["intent<br/>(클라이언트별 어휘)"]
        PR["profile<br/>(운영자 카탈로그)"]
        Q["quota"]
    end

    subgraph 물리세계["물리의 세계 (filegate 소유)"]
        F["file<br/>(정체성, 불변 id)"]
        L["location<br/>(위치, 가변 포인터)"]
        LS["lease<br/>(접근 권한, 감사 단위)"]
        PV["provider<br/>(접근 계약)"]
        CAP["capacity"]
        RC["reconciler"]
        ST["저장소"]
    end

    USER -->|인증·요청| SVC
    SVC -->|권한 확인| PERM
    SVC -->|소유| SURL
    SVC -->|등록됨| CL
    CL -->|선언| IN
    CL -->|제약받음| Q
    IN -->|참조| PR
    PR -->|후보 풀| PV
    PV -->|제약받음| CAP
    PV -->|접근| ST

    SVC -->|file_id만 영속화| F
    F -->|현재 위치| L
    L -->|놓임| PV
    LS -->|대상| F
    LS -->|예약/정산| Q
    LS -->|예약/정산| CAP
    RC -->|만료 lease 회수| LS
    RC -->|purge·이동| L
```

## 3. 관계 트리플 (온톨로지 정본)

| 주어 | 관계 | 목적어 | 출처 |
|---|---|---|---|
| 서비스 | 소유한다 | 파일의 의미 (귀속·권한·삭제 결정) | 000 공리 1 |
| filegate | 소유한다 | 파일의 물리 (위치·보존·집행) | 000 공리 1 |
| 서비스 | 영속화한다 (유일하게) | file_id | 000, 003 |
| 클라이언트 | ~이다 | 서비스의 등록 단위 | 용어집 |
| 클라이언트 | 선언한다 | intent (자기 네임스페이스) | 004 |
| intent | 참조한다 | profile | 004 |
| profile | 정의한다 | 배치 후보 풀 + 수명 정책 | 004, 001 |
| provider | ~이다 | 접근 계약 (벤더+계정+주소+자격증명+공간) | 001 |
| file | 가리킨다 (가변) | location | 001 |
| lease | 유일하게 매개한다 | 저장소에 닿는 모든 접근 | 002 |
| lease | 예약·정산한다 | quota와 capacity 양쪽 | 004 |
| 확정(commit) | 검증한다 | 선언 vs 실물 | 002 |
| reconciler | 유일하게 집행한다 | 물리 상태 변경 (회수·purge·이동) | 001, 002 |
| 중계 모드 | ~이다 | 공리 2의 선언된 예외 (capability가 강제할 때만) | 000, 002 |
| 안정 URL | 있다 | 서비스 도메인에 | 003 |
