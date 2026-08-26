#!/usr/bin/env python3
# S3 호환 표면의 실측 검증 (spec 03 완료 기준).
#
# boto3가 단일 객체 수명과 multipart 자동 전환·Abort를 완주하는지 검증하고,
# 와이어에 나간 요청 전부(메서드·경로·서명 헤더)를 기록한다. endpoint와 해당
# 대상의 자격증명·bucket을 넣어 MinIO와 filegate 양쪽에서 동일하게 통과해야
# 한다 — 그것이 표면 동등성의 정의다.
#
# 사용:
#   S3_ENDPOINT=http://127.0.0.1:9000 S3_ACCESS_KEY=… S3_SECRET_KEY=… \
#   S3_BUCKET=filegate-std python3 scripts/s3-capture.py
# filegate의 엄격한 UploadId-key 바인딩까지 강제할 때:
#   S3_EXPECT_WRONG_KEY_404=1 ... python3 scripts/s3-capture.py
#
# 의존: boto3 (pip install boto3)
import hashlib
import os
import sys
import tempfile
from pathlib import Path
from urllib.parse import urlparse

import boto3
from botocore.client import Config
from boto3.s3.transfer import TransferConfig
from botocore.exceptions import ClientError


def required_env(name):
    value = os.environ.get(name)
    if not value:
        print(f"missing required environment variable: {name}", file=sys.stderr)
        sys.exit(2)
    return value


ENDPOINT = required_env("S3_ENDPOINT")
ACCESS = required_env("S3_ACCESS_KEY")
SECRET = required_env("S3_SECRET_KEY")
BUCKET = required_env("S3_BUCKET")
EXPECT_WRONG_KEY_404 = os.environ.get("S3_EXPECT_WRONG_KEY_404") == "1"
KEY = "captest/한글 file (1).bin"  # 유니코드·공백·특수문자 키의 인코딩 실측
BODY = b"minimal op-set verification payload" * 1000
MULTIPART_KEY = "captest/multipart.bin"
ABORT_KEY = "captest/abort-bound.bin"
WRONG_ABORT_KEY = "captest/abort-wrong.bin"
MIB = 1024 * 1024

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

checks = 0
failures = 0


def check(label, condition):
    global checks, failures
    checks += 1
    if condition:
        print(f"ok   {label}")
    else:
        failures += 1
        print(f"FAIL {label}")


def sha256_file(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(MIB), b""):
            digest.update(chunk)
    return digest.digest()


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
except ClientError as error:
    check(
        "삭제 후 HEAD 404",
        error.response.get("ResponseMetadata", {}).get("HTTPStatusCode") == 404,
    )

# upload_file 기본 동작과 같은 자동 multipart 경로. 임계·chunk를 5MiB로
# 고정해 작은 검증 파일로 Create/UploadPart/Complete를 확실히 발생시킨다.
transfer = TransferConfig(
    multipart_threshold=5 * MIB,
    multipart_chunksize=5 * MIB,
    max_concurrency=3,
)
with tempfile.TemporaryDirectory(prefix="filegate-s3-capture-") as temp_dir:
    source = Path(temp_dir) / "source.bin"
    downloaded = Path(temp_dir) / "downloaded.bin"
    remaining = 11 * MIB + 123
    pattern = bytes(range(256)) * 4096
    with source.open("wb") as stream:
        while remaining > 0:
            chunk = pattern[: min(remaining, len(pattern))]
            stream.write(chunk)
            remaining -= len(chunk)

    s3.upload_file(
        str(source),
        BUCKET,
        MULTIPART_KEY,
        ExtraArgs={"ContentType": "application/octet-stream"},
        Config=transfer,
    )
    multipart_head = s3.head_object(Bucket=BUCKET, Key=MULTIPART_KEY)
    check("multipart 크기 일치", multipart_head["ContentLength"] == source.stat().st_size)
    check("multipart 합성 ETag", "-" in multipart_head["ETag"].strip('"'))
    s3.download_file(BUCKET, MULTIPART_KEY, str(downloaded))
    check("multipart download_file 본문 일치", sha256_file(downloaded) == sha256_file(source))
    s3.delete_object(Bucket=BUCKET, Key=MULTIPART_KEY)

# UploadId는 create 때의 key에 묶인다. FileGate는 다른 key에 NoSuchUpload를,
# MinIO는 204를 반환하지만 둘 다 원 세션을 건드리지 않아야 한다. 원 key에
# part를 하나 올려 생존을 확인한 뒤 Abort한다.
opened = s3.create_multipart_upload(Bucket=BUCKET, Key=ABORT_KEY)
upload_id = opened["UploadId"]
wrong_key_result = None
try:
    wrong_abort = s3.abort_multipart_upload(
        Bucket=BUCKET, Key=WRONG_ABORT_KEY, UploadId=upload_id
    )
    wrong_key_result = wrong_abort.get("ResponseMetadata", {}).get("HTTPStatusCode")
except ClientError as error:
    wrong_key_result = error.response["Error"]["Code"]
check(
    "다른 key의 Abort 응답",
    wrong_key_result == "NoSuchUpload"
    if EXPECT_WRONG_KEY_404
    else wrong_key_result in ("NoSuchUpload", 204),
)
alive_part = s3.upload_part(
    Bucket=BUCKET,
    Key=ABORT_KEY,
    UploadId=upload_id,
    PartNumber=1,
    Body=b"session-still-alive",
)
check("다른 key의 Abort 뒤 원 세션 생존", bool(alive_part.get("ETag")))
aborted = s3.abort_multipart_upload(Bucket=BUCKET, Key=ABORT_KEY, UploadId=upload_id)
check(
    "원래 key의 Abort 204",
    aborted.get("ResponseMetadata", {}).get("HTTPStatusCode") == 204,
)

print("\n와이어 실측 — 이 표면이 받아야 하는 전부:")
for method, path, query, headers in captured:
    line = f"  {method} {path}" + (f"?{query}" if query else "")
    print(line)
    for k in sorted(headers):
        print(f"      {k}: {headers[k]}")

print(f"\nchecks: {checks - failures} passed, {failures} failed ({ENDPOINT})")
sys.exit(failures)
