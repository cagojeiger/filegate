#!/usr/bin/env python3
# S3 호환 표면의 실측 검증 (spec 03 완료 기준).
#
# boto3가 Put/Head/Get(+Range)/DeleteObject만으로 전체 파일 수명을
# 완주하는지 검증하고, 와이어에 나간 요청 전부(메서드·경로·서명 헤더)를
# 기록한다. endpoint만 바꿔 MinIO와 filegate 양쪽에서 동일하게 통과해야
# 한다 — 그것이 표면 동등성의 정의다.
#
# 사용:
#   S3_ENDPOINT=http://127.0.0.1:9000 S3_ACCESS_KEY=… S3_SECRET_KEY=… \
#   S3_BUCKET=filegate-std python3 scripts/s3-capture.py
#
# 의존: boto3 (pip install boto3)
import os
import sys
from urllib.parse import urlparse

import boto3
from botocore.client import Config

ENDPOINT = os.environ.get("S3_ENDPOINT", "http://127.0.0.1:9000")
ACCESS = os.environ.get("S3_ACCESS_KEY", "filegate")
SECRET = os.environ.get("S3_SECRET_KEY", "filegate-secret")
BUCKET = os.environ.get("S3_BUCKET", "filegate-std")
KEY = "captest/한글 file (1).bin"  # 유니코드·공백·특수문자 키의 인코딩 실측
BODY = b"minimal op-set verification payload" * 1000

captured = []


def record(request, **kwargs):
    url = urlparse(request.url)
    headers = {
        k: (str(v).split(" ")[0] + " …" if k.lower() == "authorization" else v)
        for k, v in request.headers.items()
        if k.lower()
        in (
            "authorization",
            "content-type",
            "content-length",
            "x-amz-content-sha256",
            "x-amz-checksum-crc32",
            "x-amz-sdk-checksum-algorithm",
            "content-encoding",
            "expect",
            "range",
        )
    }
    captured.append((request.method, url.path, url.query, headers))


s3 = boto3.client(
    "s3",
    endpoint_url=ENDPOINT,
    aws_access_key_id=ACCESS,
    aws_secret_access_key=SECRET,
    region_name="us-east-1",
    config=Config(signature_version="s3v4", s3={"addressing_style": "path"}),
)
s3.meta.events.register("before-send.s3.*", record)

failures = 0


def check(label, condition):
    global failures
    if condition:
        print(f"ok   {label}")
    else:
        failures += 1
        print(f"FAIL {label}")


put = s3.put_object(Bucket=BUCKET, Key=KEY, Body=BODY, ContentType="application/octet-stream")
check("PutObject → ETag", bool(put.get("ETag")))

head = s3.head_object(Bucket=BUCKET, Key=KEY)
check("HeadObject 크기 일치", head["ContentLength"] == len(BODY))
check("HeadObject ETag = PUT ETag", head["ETag"] == put["ETag"])

got = s3.get_object(Bucket=BUCKET, Key=KEY)
check("GetObject 본문 일치", got["Body"].read() == BODY)

ranged = s3.get_object(Bucket=BUCKET, Key=KEY, Range="bytes=0-9")
check("Range GET 206 + 부분 본문", ranged["ResponseMetadata"]["HTTPStatusCode"] == 206 and ranged["Body"].read() == BODY[:10])

s3.delete_object(Bucket=BUCKET, Key=KEY)
try:
    s3.head_object(Bucket=BUCKET, Key=KEY)
    check("삭제 후 HEAD 404", False)
except Exception:  # noqa: BLE001 — botocore ClientError(404)
    check("삭제 후 HEAD 404", True)

print("\n와이어 실측 — 이 표면이 받아야 하는 전부:")
for method, path, query, headers in captured:
    line = f"  {method} {path}" + (f"?{query}" if query else "")
    print(line)
    for k in sorted(headers):
        print(f"      {k}: {headers[k]}")

print(f"\nchecks: {6 - failures} passed, {failures} failed ({ENDPOINT})")
sys.exit(failures)
