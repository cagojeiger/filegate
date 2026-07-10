#!/bin/sh
# 업로드 루프 E2E: create → 실제 바이트 PUT(presigned) → commit (spec 00).
#
# 전제: docker compose up (PG+MinIO), 서버 실행 중, terraform 그래프 적용
#       (tf-provider/examples — storage minio-local, client notegate,
#        키 해시, binding attachment). 로컬 개발 DB 전용.
# 사용: sh scripts/e2e-upload.sh   (종료 코드 = FAIL 수)
BASE=http://127.0.0.1:8080
RAW_KEY="fg_local-dev-notegate-key-0123456789abcdef"   # examples/main.tf의 로컬 키
AUTH="Authorization: Bearer $RAW_KEY"
JSON="Content-Type: application/json"
PG_CONTAINER="${FILEGATE_PG_CONTAINER:-filegate-postgres-1}"
PSQL="docker exec $PG_CONTAINER psql -U filegate -d filegate -qtc"
PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); }
bad() { FAIL=$((FAIL+1)); echo "FAIL: $1"; }
expect() { # $1 label, $2 want, $3 got
  if [ "$3" = "$2" ]; then ok; else bad "$1 (want $2, got $3)"; fi
}

# 시작 전 도메인 행 초기화 (회계 포함) — 로컬 개발 DB 전용
$PSQL "DELETE FROM leases;" >/dev/null 2>&1
$PSQL "DELETE FROM locations;" >/dev/null 2>&1
$PSQL "DELETE FROM files;" >/dev/null 2>&1
$PSQL "UPDATE storage_usage SET reserved_bytes=0, active_bytes=0, purge_pending_bytes=0;" >/dev/null 2>&1

PAYLOAD="hello filegate upload loop"
SIZE=$(printf '%s' "$PAYLOAD" | wc -c | tr -d ' ')
MD5=$(printf '%s' "$PAYLOAD" | md5 -q 2>/dev/null || printf '%s' "$PAYLOAD" | md5sum | cut -d' ' -f1)

echo "=== 인증 ==="
expect "인증 없음 401" 401 "$(curl -s -o /dev/null -w '%{http_code}' -X POST $BASE/v1/files -H "$JSON" -d '{}')"
expect "틀린 키 401"   401 "$(curl -s -o /dev/null -w '%{http_code}' -X POST $BASE/v1/files -H 'Authorization: Bearer fg_wrong' -H "$JSON" -d '{}')"

echo "=== create ==="
expect "없는 intent 404" 404 "$(curl -s -o /dev/null -w '%{http_code}' -H "$AUTH" -H "$JSON" -X POST $BASE/v1/files -d '{"intent":"ghost","declared_size":1}')"
expect "음수 크기 400"   400 "$(curl -s -o /dev/null -w '%{http_code}' -H "$AUTH" -H "$JSON" -X POST $BASE/v1/files -d '{"intent":"attachment","declared_size":-1}')"
expect "capacity 초과 507" 507 "$(curl -s -o /dev/null -w '%{http_code}' -H "$AUTH" -H "$JSON" -X POST $BASE/v1/files -d '{"intent":"attachment","declared_size":9999999999}')"

CREATE=$(curl -s -H "$AUTH" -H "$JSON" -X POST $BASE/v1/files \
  -d "{\"intent\":\"attachment\",\"declared_size\":$SIZE,\"content_type\":\"text/plain\",\"declared_md5\":\"$MD5\"}")
FILE_ID=$(printf '%s' "$CREATE" | sed -n 's/.*"file_id":"\([^"]*\)".*/\1/p')
PUT_URL=$(printf '%s' "$CREATE" | sed -n 's/.*"put_url":"\([^"]*\)".*/\1/p')
if [ -n "$FILE_ID" ] && [ -n "$PUT_URL" ]; then ok; else bad "create 응답에 file_id/put_url 없음: $CREATE"; fi

echo "=== 업로드 전 commit → 400 ==="
expect "실물 없음 400" 400 "$(curl -s -o /dev/null -w '%{http_code}' -H "$AUTH" -X POST $BASE/v1/files/$FILE_ID/commit)"

echo "=== 실제 바이트 PUT (presigned) ==="
expect "PUT 200" 200 "$(printf '%s' "$PAYLOAD" | curl -s -o /dev/null -w '%{http_code}' -X PUT -H 'Content-Type: text/plain' --data-binary @- "$PUT_URL")"

echo "=== commit ==="
COMMIT=$(curl -s -w '\n%{http_code}' -H "$AUTH" -X POST $BASE/v1/files/$FILE_ID/commit)
expect "commit 200" 200 "$(printf '%s' "$COMMIT" | tail -1)"
case "$COMMIT" in *"\"state\":\"active\""*) ok;; *) bad "commit 응답에 active 없음: $COMMIT";; esac
case "$COMMIT" in *"$MD5"*) ok;; *) bad "commit ETag가 MD5와 다름: $COMMIT";; esac
expect "commit 멱등 200" 200 "$(curl -s -o /dev/null -w '%{http_code}' -H "$AUTH" -X POST $BASE/v1/files/$FILE_ID/commit)"
expect "남의/없는 file commit 404" 404 "$(curl -s -o /dev/null -w '%{http_code}' -H "$AUTH" -X POST $BASE/v1/files/00000000-0000-0000-0000-000000000000/commit)"

echo "=== 크기 불일치: pending에 남는다 ==="
C2=$(curl -s -H "$AUTH" -H "$JSON" -X POST $BASE/v1/files -d '{"intent":"attachment","declared_size":999}')
F2=$(printf '%s' "$C2" | sed -n 's/.*"file_id":"\([^"]*\)".*/\1/p')
U2=$(printf '%s' "$C2" | sed -n 's/.*"put_url":"\([^"]*\)".*/\1/p')
printf '%s' "$PAYLOAD" | curl -s -o /dev/null -X PUT --data-binary @- "$U2"
expect "크기 불일치 commit 400" 400 "$(curl -s -o /dev/null -w '%{http_code}' -H "$AUTH" -X POST $BASE/v1/files/$F2/commit)"
expect "여전히 pending" "pending" "$($PSQL "SELECT state FROM files WHERE id='$F2';" | tr -d ' ')"

echo "=== 회계 검증 ==="
# 파일1 확정(active=SIZE), 파일2 예약(reserved=999)
expect "active_bytes" "$SIZE" "$($PSQL "SELECT active_bytes FROM storage_usage WHERE storage_id='minio-local';" | tr -d ' ')"
expect "reserved_bytes" "999" "$($PSQL "SELECT reserved_bytes FROM storage_usage WHERE storage_id='minio-local';" | tr -d ' ')"
expect "파일1 active" "active" "$($PSQL "SELECT state FROM files WHERE id='$FILE_ID';" | tr -d ' ')"
expect "lease 정산" "committed" "$($PSQL "SELECT state FROM leases WHERE file_id='$FILE_ID';" | tr -d ' ')"

# 정리 — 로컬 개발 DB 전용 (TF destroy가 막히지 않게)
$PSQL "DELETE FROM leases;" >/dev/null 2>&1
$PSQL "DELETE FROM locations;" >/dev/null 2>&1
$PSQL "DELETE FROM files;" >/dev/null 2>&1
$PSQL "UPDATE storage_usage SET reserved_bytes=0, active_bytes=0, purge_pending_bytes=0;" >/dev/null 2>&1

echo ""
echo "결과: PASS=$PASS FAIL=$FAIL"
exit $FAIL
