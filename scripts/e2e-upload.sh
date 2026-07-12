#!/bin/sh
# 업로드 루프 E2E: create → 실제 바이트 PUT(presigned) → commit (spec 00).
#
# 전제: docker compose up (PG+MinIO), 서버 실행 중, terraform 그래프 적용
#       (deploy/local — storage minio-local, client notegate,
#        키 해시, binding attachment). 로컬 개발 DB 전용.
# 사용: sh scripts/e2e-upload.sh   (종료 코드 = FAIL 수)
BASE=http://127.0.0.1:8080
RAW_KEY="fg_local-dev-notegate-key-0123456789abcdef"   # deploy/local/main.tf의 로컬 키
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
expect_any() { # $1 label, $2 want(공백 구분 후보), $3 got — purge 타이밍 경합 허용
  case " $2 " in *" $3 "*) ok;; *) bad "$1 (want one of [$2], got $3)";; esac
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
expect "NUL(\\u0000) intent 404(500 아님)" 404 "$(curl -s -o /dev/null -w '%{http_code}' -H "$AUTH" -H "$JSON" -X POST $BASE/v1/files -d '{"intent":"att\u0000ack","declared_size":1}')"
expect "제어문자 content_type 400" 400 "$(curl -s -o /dev/null -w '%{http_code}' -H "$AUTH" -H "$JSON" -X POST $BASE/v1/files -d '{"intent":"attachment","declared_size":1,"content_type":"a\u0000b"}')"
# 임계값 초과 선언은 multipart로 간다 (spec 02) — 크기 상한은 part×10,000.
# 1PB는 어떤 합리적 part 설정에서도 상한 밖이라 400.
expect "multipart 한계 초과 400" 400 "$(curl -s -o /dev/null -w '%{http_code}' -H "$AUTH" -H "$JSON" -X POST $BASE/v1/files -d '{"intent":"attachment","declared_size":1000000000000000}')"
expect "capacity 초과 507" 507 "$(curl -s -o /dev/null -w '%{http_code}' -H "$AUTH" -H "$JSON" -X POST $BASE/v1/files -d '{"intent":"attachment","declared_size":2147483648}')"

CREATE=$(curl -s -H "$AUTH" -H "$JSON" -X POST $BASE/v1/files \
  -d "{\"intent\":\"attachment\",\"declared_size\":$SIZE,\"content_type\":\"text/plain\",\"declared_md5\":\"$MD5\"}")
FILE_ID=$(printf '%s' "$CREATE" | sed -n 's/.*"file_id":"\([^"]*\)".*/\1/p')
PUT_URL=$(printf '%s' "$CREATE" | sed -n 's/.*"put_url":"\([^"]*\)".*/\1/p')
if [ -n "$FILE_ID" ] && [ -n "$PUT_URL" ]; then ok; else bad "create 응답에 file_id/put_url 없음: $CREATE"; fi
# 물리 배치 규약 (spec 00): 직결 URL의 키가 fg/{client}/{yyyy}/{mm}/...이어야 한다.
case "$PUT_URL" in
  */fg/notegate/20[0-9][0-9]/[0-9][0-9]/*.txt*) ok;;
  *) bad "직결 키가 규약 경로 아님: $PUT_URL";;
esac

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

echo "=== read: 올린 바이트를 도로 받는다 ==="
READ=$(curl -s -H "$AUTH" -H "$JSON" -X POST $BASE/v1/files/$FILE_ID/read -d '{"filename":"한글 파일.txt"}')
GET_URL=$(printf '%s' "$READ" | sed -n 's/.*"get_url":"\([^"]*\)".*/\1/p')
if [ -n "$GET_URL" ]; then ok; else bad "read 응답에 get_url 없음: $READ"; fi
BODY=$(curl -s "$GET_URL")
expect "다운로드 내용 일치" "$PAYLOAD" "$BODY"
DISPO=$(curl -s -o /dev/null -D - "$GET_URL" | grep -i '^content-disposition' | tr -d '\r')
case "$DISPO" in *"filename*=UTF-8''"*) ok;; *) bad "Content-Disposition RFC5987 없음: $DISPO";; esac
expect "pending 파일 read 409" 409 "$(curl -s -o /dev/null -w '%{http_code}' -H "$AUTH" -X POST $BASE/v1/files/$F2/read)"
expect "없는 파일 read 404" 404 "$(curl -s -o /dev/null -w '%{http_code}' -H "$AUTH" -X POST $BASE/v1/files/00000000-0000-0000-0000-000000000000/read)"

echo "=== stat ==="
STAT=$(curl -s -H "$AUTH" $BASE/v1/files/$FILE_ID)
case "$STAT" in *'"state":"active"'*) ok;; *) bad "stat active 아님: $STAT";; esac
case "$STAT" in *"\"declared_size\":$SIZE"*) ok;; *) bad "stat 크기 불일치: $STAT";; esac
case "$STAT" in *'"intent":"attachment"'*) ok;; *) bad "stat intent 불일치: $STAT";; esac
expect "read lease 원장 기록" "1" "$($PSQL "SELECT count(*) FROM leases WHERE file_id='$FILE_ID' AND kind='read';" | tr -d ' ')"

echo "=== 회계 검증 ==="
# 파일1 확정(active=SIZE), 파일2 예약(reserved=999)
expect "active_bytes" "$SIZE" "$($PSQL "SELECT active_bytes FROM storage_usage WHERE storage_id='minio-local';" | tr -d ' ')"
expect "reserved_bytes" "999" "$($PSQL "SELECT reserved_bytes FROM storage_usage WHERE storage_id='minio-local';" | tr -d ' ')"
expect "파일1 active" "active" "$($PSQL "SELECT state FROM files WHERE id='$FILE_ID';" | tr -d ' ')"
expect "쓰기 lease 정산" "committed" "$($PSQL "SELECT state FROM leases WHERE file_id='$FILE_ID' AND kind='write';" | tr -d ' ')"

echo "=== delete(detach) → reconciler purge ==="
DEL=$(curl -s -w '\n%{http_code}' -H "$AUTH" -X DELETE $BASE/v1/files/$FILE_ID)
expect "delete 200" 200 "$(printf '%s' "$DEL" | tail -1)"
expect "delete 멱등 200" 200 "$(curl -s -o /dev/null -w '%{http_code}' -H "$AUTH" -X DELETE $BASE/v1/files/$FILE_ID)"
# purge(tick 2초)가 검사보다 먼저 돌 수 있다 — 계약상 purge 전 409, 후 404.
expect_any "삭제 후 read 409|404" "409 404" "$(curl -s -o /dev/null -w '%{http_code}' -H "$AUTH" -X POST $BASE/v1/files/$FILE_ID/read)"
expect_any "삭제 후 commit 409|404" "409 404" "$(curl -s -o /dev/null -w '%{http_code}' -H "$AUTH" -X POST $BASE/v1/files/$FILE_ID/commit)"
expect "pending 파일 delete 409" 409 "$(curl -s -o /dev/null -w '%{http_code}' -H "$AUTH" -X DELETE $BASE/v1/files/$F2)"
expect_any "purge 대기 회계(대기중|정리됨)" "$SIZE 0" "$($PSQL "SELECT purge_pending_bytes FROM storage_usage WHERE storage_id='minio-local';" | tr -d ' ')"

echo "=== reconciler: 만료 회수 + purge (tick 대기) ==="
# pending 파일(F2)의 쓰기 lease를 강제 만료시킨다 (테스트 전용)
$PSQL "UPDATE leases SET expires_at = now() - interval '1 second' WHERE file_id='$F2' AND kind='write';" >/dev/null
sleep 7   # FILEGATE_RECONCILER_INTERVAL_SECS=2 기준 tick 3회 이상
expect "pending → reclaimed" "reclaimed" "$($PSQL "SELECT state FROM files WHERE id='$F2';" | tr -d ' ')"
expect "회수된 파일 stat 404 (내부 상태 비노출)" 404 "$(curl -s -o /dev/null -w '%{http_code}' -H "$AUTH" $BASE/v1/files/$F2)"
expect "회수된 파일 delete 404 (일관성)" 404 "$(curl -s -o /dev/null -w '%{http_code}' -H "$AUTH" -X DELETE $BASE/v1/files/$F2)"
expect "회수 후 reserved 0" "0" "$($PSQL "SELECT reserved_bytes FROM storage_usage WHERE storage_id='minio-local';" | tr -d ' ')"
expect "purge 후 대기 0" "0" "$($PSQL "SELECT purge_pending_bytes FROM storage_usage WHERE storage_id='minio-local';" | tr -d ' ')"
expect "purge 후에도 stat은 답한다(deleted)" "deleted" "$($PSQL "SELECT state FROM files WHERE id='$FILE_ID';" | tr -d ' ')"
expect "location 제거됨" "0" "$($PSQL "SELECT count(*) FROM locations;" | tr -d ' ')"
DL=$(curl -s -o /dev/null -w '%{http_code}' "$GET_URL")
expect "purge 후 기존 GET URL 404" 404 "$DL"

# 정리 — 로컬 개발 DB 전용 (TF destroy가 막히지 않게)
$PSQL "DELETE FROM leases;" >/dev/null 2>&1
$PSQL "DELETE FROM locations;" >/dev/null 2>&1
$PSQL "DELETE FROM files;" >/dev/null 2>&1
$PSQL "UPDATE storage_usage SET reserved_bytes=0, active_bytes=0, purge_pending_bytes=0;" >/dev/null 2>&1

echo ""
echo "결과: PASS=$PASS FAIL=$FAIL"
exit $FAIL
