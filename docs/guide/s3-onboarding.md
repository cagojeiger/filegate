# S3 연동

계약: [S3 spec](../spec/03-s3-surface.md)

## 연결 정보

| 항목 | 예 |
|---|---|
| endpoint | https://filegate.internal |
| access key·secret | 운영자 API에서 client에 발급 |
| bucket | client id |
| 주소 방식 | path-style |
| region | us-east-1; SigV4 scope에서 일관되게 사용 |

## boto3

```python
import boto3
from botocore.client import Config

s3 = boto3.client(
    "s3",
    endpoint_url=FILEGATE_S3_ENDPOINT,
    aws_access_key_id=ACCESS_KEY,
    aws_secret_access_key=SECRET_KEY,
    region_name="us-east-1",
    config=Config(signature_version="s3v4", s3={"addressing_style": "path"}),
)

s3.put_object(Bucket=BUCKET, Key="reports/2026/07.pdf", Body=data,
              ContentType="application/pdf")
s3.head_object(Bucket=BUCKET, Key="reports/2026/07.pdf")
s3.get_object(Bucket=BUCKET, Key="reports/2026/07.pdf")
s3.delete_object(Bucket=BUCKET, Key="reports/2026/07.pdf")
```

서비스는 bucket·logical key를 저장한다. 같은 key에 PUT하면 새 객체로 교체한다.
바이트는 FileGate를 거쳐 저장소에 기록된다.

| 사용 | 현재 계약 |
|---|---|
| 작은 객체 | Put·Head·Get·Delete |
| 큰 객체 | SDK upload_file/download_file의 multipart·Range |
| 중단·재시도 | Complete·Abort 상태와 복구 재료를 원장에 보존 |
| presigned URL | SDK가 생성한 query-signed SigV4 검증 |
| 목록·버킷 관리 API | 지원 범위 밖; 사용할 클라이언트의 요구 확인 |

## 검증

[s3-capture.py](../../scripts/s3-capture.py)에 S3_ENDPOINT, S3_ACCESS_KEY,
S3_SECRET_KEY, S3_BUCKET을 공급한다. 객체 수명·Range·자동 multipart·Abort를 확인하며,
FileGate에는 S3_EXPECT_WRONG_KEY_404=1을 더해 다른 key의 Abort 계약을 검증한다.
