#!/bin/sh
# 3-모드 동등성 E2E (완료 조건): minio 직결 = minio 중계 = fs 중계.
# 같은 시나리오를 세 binding으로 돌려 상태 전이·회계·응답이 동일함을 검증하고,
# 중계 전용 강화 케이스(secret·CL·CORS·kind 교차)를 추가로 찌른다.
#
# 전제: docker compose up, 서버 실행 중(FILEGATE_PUBLIC_URL 필수, tick 짧게),
#       terraform 그래프 적용(deploy/local — 3 storage + 3 binding),
#       /tmp/filegate-fs-demo 존재. 로컬 개발 DB 전용.
# 사용: sh scripts/e2e-relay.sh   (종료 코드 = FAIL 수)
BASE=http://127.0.0.1:8080
RAW_KEY="fg_local-dev-notegate-key-0123456789abcdef"
AUTH="Authorization: Bearer $RAW_KEY"
JSON="Content-Type: application/json"
PG_CONTAINER="${FILEGATE_PG_CONTAINER:-filegate-postgres-1}"
PSQL="docker exec $PG_CONTAINER psql -U filegate -d filegate -qtc"
PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); }
bad() { FAIL=$((FAIL+1)); echo "FAIL: $1"; }
expect() { if [ "$3" = "$2" ]; then ok; else bad "$1 (want $2, got $3)"; fi }

# 시작 전 도메인 초기화
$PSQL "DELETE FROM leases;" >/dev/null 2>&1
$PSQL "DELETE FROM locations;" >/dev/null 2>&1
$PSQL "DELETE FROM files;" >/dev/null 2>&1
$PSQL "UPDATE storage_usage SET reserved_bytes=0, active_bytes=0, purge_pending_bytes=0;" >/dev/null 2>&1
mkdir -p /tmp/filegate-fs-demo
rm -rf /tmp/filegate-fs-demo/fg /tmp/filegate-fs-demo/.fg-tmp-* 2>/dev/null

# fs 실물 파일 수 — 물리 배치(fg/{client}/{yyyy}/{mm}/{zz}/...)가 중첩이라
# 재귀로 센다. 임시(.fg-tmp-*)는 제외.
fs_count() { find /tmp/filegate-fs-demo -type f ! -name '.fg-tmp-*' | wc -l | tr -d ' '; }

md5of() { printf '%s' "$1" | md5 -q 2>/dev/null || printf '%s' "$1" | md5sum | cut -d' ' -f1; }

# ── 공통 시나리오: create→PUT→commit→read→GET→stat. 모드당 동일 ──
# $1 intent, $2 storage_id, $3 URL 판별(direct|relay), $4 페이로드
run_mode() {
  INTENT=$1; SID=$2; URLKIND=$3; PAYLOAD=$4
  SIZE=$(printf '%s' "$PAYLOAD" | wc -c | tr -d ' ')
  MD5=$(md5of "$PAYLOAD")
  echo "--- [$INTENT → $SID / $URLKIND]"
  C=$(curl -s -H "$AUTH" -H "$JSON" -X POST $BASE/v1/files \
    -d "{\"intent\":\"$INTENT\",\"declared_size\":$SIZE,\"content_type\":\"text/plain\",\"declared_md5\":\"$MD5\"}")
  FID=$(printf '%s' "$C" | sed -n 's/.*"file_id":"\([^"]*\)".*/\1/p')
  PURL=$(printf '%s' "$C" | sed -n 's/.*"put_url":"\([^"]*\)".*/\1/p')
  if [ -n "$FID" ] && [ -n "$PURL" ]; then ok; else bad "[$INTENT] create 실패: $C"; return; fi
  case "$URLKIND" in
    direct) case "$PURL" in http://127.0.0.1:9000/*) ok;; *) bad "[$INTENT] 직결 URL 아님: $PURL";; esac;;
    relay)  case "$PURL" in $BASE/b/*\?s=*) ok;; *) bad "[$INTENT] 중계 URL 아님: $PURL";; esac;;
  esac
  expect "[$INTENT] PUT 200" 200 "$(printf '%s' "$PAYLOAD" | curl -s -o /dev/null -w '%{http_code}' -X PUT -H 'Content-Type: text/plain' --data-binary @- "$PURL")"
  CM=$(curl -s -w '\n%{http_code}' -H "$AUTH" -X POST $BASE/v1/files/$FID/commit)
  expect "[$INTENT] commit 200" 200 "$(printf '%s' "$CM" | tail -1)"
  case "$CM" in *"$MD5"*) ok;; *) bad "[$INTENT] commit ETag != MD5: $CM";; esac
  R=$(curl -s -H "$AUTH" -H "$JSON" -X POST $BASE/v1/files/$FID/read -d '{"filename":"모드 검증 v1+2&3#final.txt"}')
  GURL=$(printf '%s' "$R" | sed -n 's/.*"get_url":"\([^"]*\)".*/\1/p')
  if [ -n "$GURL" ]; then ok; else bad "[$INTENT] read 실패: $R"; return; fi
  expect "[$INTENT] 다운로드 내용 일치" "$PAYLOAD" "$(curl -s "$GURL")"
  DISPO=$(curl -s -o /dev/null -D - "$GURL" | grep -i '^content-disposition' | tr -d '\r')
  case "$DISPO" in *"filename*=UTF-8''"*) ok;; *) bad "[$INTENT] RFC5987 없음: $DISPO";; esac
  # 파일명 URL 왕복 무결: &(절단)·+(공백 변질)·#(fragment 소실)이 살아남아야 한다.
  case "$DISPO" in *"v1+2&3#final.txt"*) ok;; *) bad "[$INTENT] 파일명 악문자 왕복 실패: $DISPO";; esac
  expect "[$INTENT] 회계 active" "$SIZE" "$($PSQL "SELECT active_bytes FROM storage_usage WHERE storage_id='$SID';" | tr -d ' ')"
  ST=$(curl -s -H "$AUTH" $BASE/v1/files/$FID)
  case "$ST" in *'"state":"active"'*) ok;; *) bad "[$INTENT] stat: $ST";; esac
  eval "FID_$(printf '%s' "$INTENT" | tr - _)=$FID"
  eval "GURL_$(printf '%s' "$INTENT" | tr - _)='$GURL'"
}

echo "=== 3-모드 동등성 ==="
run_mode attachment minio-local  direct "동등성 페이로드 — 직결 minio"
run_mode relay-att  minio-relay  relay  "동등성 페이로드 — 중계 minio"
run_mode fs-att     fs-local     relay  "동등성 페이로드 — 중계 fs"

echo "=== fs 실물 확인 (root_path에 파일이 실제로, 규약 경로로) ==="
expect "fs 객체 1개" 1 "$(fs_count)"
# 물리 배치 규약 검증 (spec 00): fg/{client}/{yyyy}/{mm}/{zz}/{uuid}[.ext]
FSPATH=$(find /tmp/filegate-fs-demo -type f ! -name '.fg-tmp-*' | head -1)
case "$FSPATH" in
  /tmp/filegate-fs-demo/fg/notegate/20[0-9][0-9]/[0-9][0-9]/??/*) ok;;
  *) bad "fs 키가 규약 경로 아님: $FSPATH";;
esac

echo "=== 중계 강화 케이스 ==="
# 새 중계 파일 하나로 secret 계열 공격
C=$(curl -s -H "$AUTH" -H "$JSON" -X POST $BASE/v1/files -d '{"intent":"relay-att","declared_size":10}')
FID3=$(printf '%s' "$C" | sed -n 's/.*"file_id":"\([^"]*\)".*/\1/p')
PURL3=$(printf '%s' "$C" | sed -n 's/.*"put_url":"\([^"]*\)".*/\1/p')
LEASE3=$(printf '%s' "$PURL3" | sed -n 's|.*/b/\([^?]*\).*|\1|p')
expect "틀린 secret 403" 403 "$(printf '0123456789' | curl -s -o /dev/null -w '%{http_code}' -X PUT --data-binary @- "$BASE/b/$LEASE3?s=wrongsecret")"
expect "없는 lease 403" 403 "$(printf '0123456789' | curl -s -o /dev/null -w '%{http_code}' -X PUT --data-binary @- "$BASE/b/00000000-0000-0000-0000-000000000000?s=wrongsecret")"
# 읽기 lease의 secret으로 PUT (kind 교차) → 403
GURL_CROSS=$(eval echo "\$GURL_relay_att")
RLEASE=$(printf '%s' "$GURL_CROSS" | sed -n 's|.*/b/\([^?]*\).*|\1|p')
RSECRET=$(printf '%s' "$GURL_CROSS" | sed -n 's|.*?s=\(.*\)|\1|p')
expect "read secret으로 PUT 403" 403 "$(printf '0123456789' | curl -s -o /dev/null -w '%{http_code}' -X PUT --data-binary @- "$BASE/b/$RLEASE?s=$RSECRET")"
# CL 검증
expect "CL != 선언크기 400" 400 "$(printf '12345' | curl -s -o /dev/null -w '%{http_code}' -X PUT --data-binary @- "$PURL3")"
expect "chunked(CL 없음) 411" 411 "$(printf '0123456789' | curl -s -o /dev/null -w '%{http_code}' -X PUT -H 'Transfer-Encoding: chunked' --data-binary @- "$PURL3")"
# 만료 lease → 403
$PSQL "UPDATE leases SET expires_at = now() - interval '1 second' WHERE id='$LEASE3';" >/dev/null
expect "만료 lease PUT 403" 403 "$(printf '0123456789' | curl -s -o /dev/null -w '%{http_code}' -X PUT --data-binary @- "$PURL3")"
# CORS preflight
PF=$(curl -s -o /dev/null -D - -X OPTIONS "$BASE/b/$LEASE3" | tr -d '\r')
case "$PF" in *"access-control-allow-origin: *"*|*"Access-Control-Allow-Origin: *"*) ok;; *) bad "preflight CORS 헤더 없음: $PF";; esac
# md5 불일치 (중계 검증 경로): 선언 md5와 다른 내용 업로드 → commit 400
WRONGMD5=$(md5of "다른 내용")
C4=$(curl -s -H "$AUTH" -H "$JSON" -X POST $BASE/v1/files -d "{\"intent\":\"fs-att\",\"declared_size\":9,\"declared_md5\":\"$WRONGMD5\"}")
F4=$(printf '%s' "$C4" | sed -n 's/.*"file_id":"\([^"]*\)".*/\1/p')
U4=$(printf '%s' "$C4" | sed -n 's/.*"put_url":"\([^"]*\)".*/\1/p')
printf '123456789' | curl -s -o /dev/null -X PUT --data-binary @- "$U4"
expect "중계 md5 불일치 commit 400" 400 "$(curl -s -o /dev/null -w '%{http_code}' -H "$AUTH" -X POST $BASE/v1/files/$F4/commit)"
expect "불일치 후 pending 유지" "pending" "$($PSQL "SELECT state FROM files WHERE id='$F4';" | tr -d ' ')"

# 불일치 파일(F4)은 pending으로 남는 게 계약 — 회수 경로로 정리한다
# (fs 백엔드의 reclaim sweep 검증을 겸함).
$PSQL "UPDATE leases SET expires_at = now() - interval '1 second' WHERE file_id='$F4' AND kind='write';" >/dev/null

echo "=== 세 모드 delete → purge + F4 회수 → 회계 0 + 중계 GET 404 ==="
for I in attachment relay_att fs_att; do
  FID=$(eval echo "\$FID_$I")
  expect "[$I] delete 200" 200 "$(curl -s -o /dev/null -w '%{http_code}' -H "$AUTH" -X DELETE $BASE/v1/files/$FID)"
done
sleep 8
expect "회계 전부 0" "0" "$($PSQL "SELECT coalesce(sum(reserved_bytes+active_bytes+purge_pending_bytes),0) FROM storage_usage WHERE storage_id IN ('minio-relay','fs-local');" | tr -d ' ')"
GURL_R=$(eval echo "\$GURL_relay_att")
expect "purge 후 중계 GET 404" 404 "$(curl -s -o /dev/null -w '%{http_code}' "$GURL_R")"
expect "fs 실물 전부 소멸" 0 "$(fs_count)"

# 정리
$PSQL "DELETE FROM leases;" >/dev/null 2>&1
$PSQL "DELETE FROM locations;" >/dev/null 2>&1
$PSQL "DELETE FROM files;" >/dev/null 2>&1
$PSQL "UPDATE storage_usage SET reserved_bytes=0, active_bytes=0, purge_pending_bytes=0;" >/dev/null 2>&1

echo ""
echo "결과: PASS=$PASS FAIL=$FAIL"
exit $FAIL
