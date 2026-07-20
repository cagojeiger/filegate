# S3 호환 온보딩 — filegate를 "S3 하나 더"로 쓴다

기존 S3 코드를 가진 서비스가 filegate에 붙는 가장 짧은 길이다. 코드는
그대로 두고 endpoint·자격증명·버킷 이름만 바꾼다. 대가는 바이트가
filegate를 지나는 것이다 (ADR [006](../adr/006-s3-compat-surface.md) —
온보딩 계층). 트래픽 비용이 중요해지면 [네이티브 연동](service-integration.md)으로
서비스 단위로 갈아탄다 — 파일 장부는 그대로다.

## 받는 것 (운영자에게 요청)

| 항목 | 예 |
|---|---|
| endpoint | `https://filegate.internal` (컨트롤과 같은 리스너 — 전용 포트 없음) |
| access key id / secret key | 등록부가 발급 |
| 버킷 이름 | 서비스의 client id (버킷 = client id, 예: `notegate`) |

## 붙는 법 (boto3)

```python
import boto3
from botocore.client import Config

s3 = boto3.client(
    "s3",
    endpoint_url=FILEGATE_S3_ENDPOINT,
    aws_access_key_id=ACCESS_KEY,
    aws_secret_access_key=SECRET_KEY,
    region_name="us-east-1",          # 서명 재료일 뿐 — 이 값으로 고정
    config=Config(signature_version="s3v4", s3={"addressing_style": "path"}),
)

s3.put_object(Bucket=BUCKET, Key="reports/2026/07.pdf", Body=data,
              ContentType="application/pdf")
s3.head_object(Bucket=BUCKET, Key="reports/2026/07.pdf")
s3.get_object(Bucket=BUCKET, Key="reports/2026/07.pdf")
s3.delete_object(Bucket=BUCKET, Key="reports/2026/07.pdf")
```

key는 서비스가 정한다 — 유니코드·공백·특수문자 그대로 쓴다. 같은 key에
다시 PUT하면 덮어쓰기다 (S3와 동일).

## 지원 범위 ([spec 03](../spec/03-s3-surface.md))

- **PutObject / HeadObject / GetObject(+Range) / DeleteObject** — 작은 파일의
  전체 수명에 충분함이 실측이다.
- **multipart 지원** — `upload_file`의 임계 초과 자동 전환(Create/UploadPart/
  Complete/Abort)이 그대로 동작한다. part 크기·개수는 SDK가 정하며 filegate가
  실측으로 검증한다 — 큰 파일 업로드·다운로드(`upload_file`/`download_file`)를
  기본값으로 쓰면 된다.
- **ListObjects는 없다** — 어떤 key를 썼는지는 서비스 DB가 안다. 목록이
  필요한 설계라면 네이티브 연동이 맞다.

## 확인

[scripts/s3-capture.py](../../scripts/s3-capture.py)를 endpoint만 바꿔
실행하면 전체 수명(업로드→확인→다운로드→Range→삭제→404)이 검증된다.
