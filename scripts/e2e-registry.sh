#!/bin/sh
# 등록부 검증 스위트: A. DB 제약 프로브(직접 SQL) / B. 운영자 API E2E(curl).
#
# 전제: docker compose up (PG + MinIO), 서버 실행 중(cargo run -p filegate-api),
#       .env의 로컬 개발 토큰(fgop_local-dev). 등록부 테이블을 비우고 시작한다 —
#       로컬 개발 DB 전용이다.
# 사용: sh scripts/e2e-registry.sh   (종료 코드 = FAIL 수)
BASE=http://127.0.0.1:8080
AUTH="Authorization: Bearer fgop_local-dev"
JSON="Content-Type: application/json"
# compose 프로젝트/서비스 이름이 다르면 FILEGATE_PG_CONTAINER로 지정한다.
PG_CONTAINER="${FILEGATE_PG_CONTAINER:-filegate-postgres-1}"
PSQL="docker exec $PG_CONTAINER psql -U filegate -d filegate -qc"
PASS=0; FAIL=0

ok()   { PASS=$((PASS+1)); }
bad()  { FAIL=$((FAIL+1)); echo "FAIL: $1"; }
sqlfail() { if $PSQL "$2" >/dev/null 2>&1; then bad "(SQL 거부돼야 함) $1"; else ok; fi }
sqlok()   { if $PSQL "$2" >/dev/null 2>&1; then ok; else bad "(SQL 성공해야 함) $1"; fi }
http() { # $1 label, $2 expected, 이후 curl 인자
  label=$1; want=$2; shift 2
  got=$(curl -s -o /dev/null -w '%{http_code}' "$@")
  if [ "$got" = "$want" ]; then ok; else bad "$label (want $want, got $got)"; fi
}

NONCE="decode(repeat('00',12),'hex')"; CT="decode('deadbeef','hex')"
HASH_A="sha256:$(printf 'a%.0s' $(seq 64))"
HASH_B="sha256:$(printf 'b%.0s' $(seq 64))"

# 시작 전 등록부 초기화 (문장별 개별 실행 — 소유물/참조 → 노드)
$PSQL "DELETE FROM lease_parts;" >/dev/null 2>&1
$PSQL "DELETE FROM leases;" >/dev/null 2>&1
$PSQL "DELETE FROM locations;" >/dev/null 2>&1
$PSQL "DELETE FROM files;" >/dev/null 2>&1
$PSQL "DELETE FROM s3_credentials;" >/dev/null 2>&1
$PSQL "DELETE FROM client_keys;" >/dev/null 2>&1
$PSQL "DELETE FROM clients;" >/dev/null 2>&1
$PSQL "DELETE FROM storages;" >/dev/null 2>&1

echo "=== A. DB 제약 프로브 ==="
sqlfail "storage 슬러그 대문자" "INSERT INTO storages (id,endpoint,public_endpoint,region,bucket,force_path_style,access_key,secret_key_ciphertext,secret_key_nonce,enc_key_id,capacity_bytes) VALUES ('Bad_ID','e','e','r','b',false,'ak',$CT,$NONCE,'v1',0);"
sqlfail "nonce 11바이트" "INSERT INTO storages (id,endpoint,public_endpoint,region,bucket,force_path_style,access_key,secret_key_ciphertext,secret_key_nonce,enc_key_id,capacity_bytes) VALUES ('s1','e','e','r','b',false,'ak',$CT,decode(repeat('00',11),'hex'),'v1',0);"
sqlfail "capacity 음수" "INSERT INTO storages (id,endpoint,public_endpoint,region,bucket,force_path_style,access_key,secret_key_ciphertext,secret_key_nonce,enc_key_id,capacity_bytes) VALUES ('s1','e','e','r','b',false,'ak',$CT,$NONCE,'v1',-1);"
sqlok   "storage 정상" "INSERT INTO storages (id,endpoint,public_endpoint,region,bucket,force_path_style,access_key,secret_key_ciphertext,secret_key_nonce,enc_key_id,capacity_bytes) VALUES ('s1','e','e','r','b',false,'ak',$CT,$NONCE,'v1',10);"
sqlfail "storage id 중복" "INSERT INTO storages (id,endpoint,public_endpoint,region,bucket,force_path_style,access_key,secret_key_ciphertext,secret_key_nonce,enc_key_id,capacity_bytes) VALUES ('s1','e','e','r','b',false,'ak',$CT,$NONCE,'v1',10);"
sqlok   "client 정상" "INSERT INTO clients (id,storage_id) VALUES ('c1','s1');"
sqlfail "client 슬러그 위반" "INSERT INTO clients (id,storage_id) VALUES ('-bad','s1');"
sqlfail "client의 없는 storage" "INSERT INTO clients (id,storage_id) VALUES ('ghost','missing');"
sqlfail "key 해시 형식 위반" "INSERT INTO client_keys (key_hash,client_id) VALUES ('sha256:zzz','c1');"
sqlok   "key 정상" "INSERT INTO client_keys (key_hash,client_id) VALUES ('$HASH_A','c1');"
sqlok   "둘째 client" "INSERT INTO clients (id,storage_id) VALUES ('c2','s1');"
sqlfail "key 해시 전역 중복(다른 client라도)" "INSERT INTO client_keys (key_hash,client_id) VALUES ('$HASH_A','c2');"
sqlfail "client가 참조하는 storage 삭제" "DELETE FROM storages WHERE id='s1';"
sqlfail "미등록 client의 file" "INSERT INTO files (client_id,declared_size) VALUES ('ghost',1);"
sqlok   "등록 client의 file" "INSERT INTO files (client_id,declared_size) VALUES ('c1',1);"
sqlfail "file 남은 client 삭제" "DELETE FROM clients WHERE id='c1';"
sqlok   "file 정리" "DELETE FROM files WHERE client_id='c1';"
sqlok   "client 삭제 → key cascade" "DELETE FROM clients WHERE id='c1';"
LEFT=$($PSQL "SELECT count(*) FROM client_keys WHERE client_id='c1';" -t | tr -d ' \n')
if [ "$LEFT" = "0" ]; then ok; else bad "key cascade 잔여 $LEFT"; fi
sqlok   "정리: c2" "DELETE FROM clients WHERE id='c2';"
sqlok   "정리: s1" "DELETE FROM storages WHERE id='s1';"

echo "=== B. 운영자 API E2E ==="
S='{"endpoint":"http://127.0.0.1:9000","region":"us-east-1","bucket":"filegate-std","force_path_style":true,"access_key":"filegate","secret_key":"filegate-secret","capacity_bytes":1073741824}'
SBAD='{"endpoint":"http://127.0.0.1:9000","region":"us-east-1","bucket":"filegate-std","force_path_style":true,"access_key":"filegate","secret_key":"wrong","capacity_bytes":1}'
http "인증 없음 401"        401 $BASE/api/admin/v1/storages
http "틀린 토큰 401"        401 -H "Authorization: Bearer nope" $BASE/api/admin/v1/storages
http "storage 틀린시크릿 400" 400 -H "$AUTH" -H "$JSON" -X POST $BASE/api/admin/v1/storages -d "{\"id\":\"minio-a\",${SBAD#\{}"
http "storage 생성 201"     201 -H "$AUTH" -H "$JSON" -X POST $BASE/api/admin/v1/storages -d "{\"id\":\"minio-a\",${S#\{}"
http "storage 중복 409"     409 -H "$AUTH" -H "$JSON" -X POST $BASE/api/admin/v1/storages -d "{\"id\":\"minio-a\",${S#\{}"
http "storage 나쁜슬러그 400" 400 -H "$AUTH" -H "$JSON" -X POST $BASE/api/admin/v1/storages -d "{\"id\":\"Bad_ID\",${S#\{}"
http "storage 둘째 생성 201" 201 -H "$AUTH" -H "$JSON" -X POST $BASE/api/admin/v1/storages -d "{\"id\":\"minio-b\",${S#\{}"
http "storage 조회 200"     200 -H "$AUTH" $BASE/api/admin/v1/storages/minio-a
http "storage 없는 조회 404" 404 -H "$AUTH" $BASE/api/admin/v1/storages/ghost
http "storage 갱신 200"     200 -H "$AUTH" -H "$JSON" -X PUT $BASE/api/admin/v1/storages/minio-a -d "$S"
http "storage 없는 갱신 404" 404 -H "$AUTH" -H "$JSON" -X PUT $BASE/api/admin/v1/storages/ghost -d "$S"
http "client storage_id 누락 422" 422 -H "$AUTH" -H "$JSON" -X POST $BASE/api/admin/v1/clients -d '{"id":"notegate"}'
http "client 없는 storage 400" 400 -H "$AUTH" -H "$JSON" -X POST $BASE/api/admin/v1/clients -d '{"id":"ghost","storage_id":"missing"}'
http "client 생성 201"      201 -H "$AUTH" -H "$JSON" -X POST $BASE/api/admin/v1/clients -d '{"id":"notegate","storage_id":"minio-b"}'
http "client 중복 409"      409 -H "$AUTH" -H "$JSON" -X POST $BASE/api/admin/v1/clients -d '{"id":"notegate","storage_id":"minio-b"}'
http "client 조회 200"      200 -H "$AUTH" $BASE/api/admin/v1/clients/notegate
http "client 없는 조회 404" 404 -H "$AUTH" $BASE/api/admin/v1/clients/ghost
CLIENT=$(curl -s -H "$AUTH" $BASE/api/admin/v1/clients/notegate)
case "$CLIENT" in *'"storage_id":"minio-b"'*) ok;; *) bad "client storage_id 불일치: $CLIENT";; esac
http "client storage 변경 미지원 405" 405 -H "$AUTH" -H "$JSON" -X PUT $BASE/api/admin/v1/clients/notegate -d '{"storage_id":"minio-a"}'
http "key 등록 201"         201 -H "$AUTH" -H "$JSON" -X POST $BASE/api/admin/v1/clients/notegate/keys -d "{\"key_hash\":\"$HASH_A\"}"
http "key 중복 409"         409 -H "$AUTH" -H "$JSON" -X POST $BASE/api/admin/v1/clients/notegate/keys -d "{\"key_hash\":\"$HASH_A\"}"
http "key 형식위반 400"     400 -H "$AUTH" -H "$JSON" -X POST $BASE/api/admin/v1/clients/notegate/keys -d '{"key_hash":"sha256:short"}'
http "key 없는client 404"   404 -H "$AUTH" -H "$JSON" -X POST $BASE/api/admin/v1/clients/ghost/keys -d "{\"key_hash\":\"$HASH_B\"}"
http "key 회전: 둘째 201"   201 -H "$AUTH" -H "$JSON" -X POST $BASE/api/admin/v1/clients/notegate/keys -d "{\"key_hash\":\"$HASH_B\"}"
http "key 조회 200"         200 -H "$AUTH" $BASE/api/admin/v1/clients/notegate/keys/$HASH_A
http "key 첫째 삭제 204"    204 -H "$AUTH" -X DELETE $BASE/api/admin/v1/clients/notegate/keys/$HASH_A
http "key 삭제 멱등 204"    204 -H "$AUTH" -X DELETE $BASE/api/admin/v1/clients/notegate/keys/$HASH_A
http "key 삭제후 조회 404"  404 -H "$AUTH" $BASE/api/admin/v1/clients/notegate/keys/$HASH_A
CRED=$(curl -s -w '\n%{http_code}' -H "$AUTH" -X POST $BASE/api/admin/v1/clients/notegate/s3-credentials)
CRED_BODY=$(printf '%s\n' "$CRED" | sed '$d')
CRED_ID=$(printf '%s' "$CRED_BODY" | sed -n 's/.*"access_key_id":"\([^"]*\)".*/\1/p')
CRED_SECRET=$(printf '%s' "$CRED_BODY" | sed -n 's/.*"secret_key":"\([^"]*\)".*/\1/p')
if [ "$(printf '%s\n' "$CRED" | tail -1)" = "201" ]; then ok; else bad "S3 credential 생성 상태: $CRED"; fi
if [ -n "$CRED_ID" ] && [ -n "$CRED_SECRET" ]; then ok; else bad "S3 credential 1회 secret 응답 누락: $CRED_BODY"; fi
CRED_LIST=$(curl -s -H "$AUTH" $BASE/api/admin/v1/clients/notegate/s3-credentials)
case "$CRED_LIST" in *"$CRED_ID"*) ok;; *) bad "S3 credential 목록 누락: $CRED_LIST";; esac
http "S3 credential 삭제 204" 204 -H "$AUTH" -X DELETE $BASE/api/admin/v1/clients/notegate/s3-credentials/$CRED_ID
http "S3 credential 삭제 멱등 204" 204 -H "$AUTH" -X DELETE $BASE/api/admin/v1/clients/notegate/s3-credentials/$CRED_ID
CRED2=$(curl -s -H "$AUTH" -X POST $BASE/api/admin/v1/clients/notegate/s3-credentials)
CRED2_ID=$(printf '%s' "$CRED2" | sed -n 's/.*"access_key_id":"\([^"]*\)".*/\1/p')
if [ -n "$CRED2_ID" ]; then ok; else bad "cascade 검증용 S3 credential 생성 실패: $CRED2"; fi
http "소문자 bearer 허용 200" 200 -H "authorization: bearer fgop_local-dev" $BASE/api/admin/v1/storages
http "capacity 음수 400(네트워크 검증 전)" 400 -H "$AUTH" -H "$JSON" -X POST $BASE/api/admin/v1/storages -d '{"id":"neg","endpoint":"http://127.0.0.1:1","region":"r","bucket":"b","access_key":"a","secret_key":"s","capacity_bytes":-1}'
http "없는 storage 갱신 404(네트워크 검증 전)" 404 -H "$AUTH" -H "$JSON" -X PUT $BASE/api/admin/v1/storages/ghost2 -d '{"endpoint":"http://127.0.0.1:1","region":"r","bucket":"b","access_key":"a","secret_key":"s","capacity_bytes":1}'
http "사용중 storage-b 삭제 409" 409 -H "$AUTH" -X DELETE $BASE/api/admin/v1/storages/minio-b
http "미사용 storage-a 삭제 204" 204 -H "$AUTH" -X DELETE $BASE/api/admin/v1/storages/minio-a
sqlok "client 소유 file 생성" "INSERT INTO files (client_id,declared_size) VALUES ('notegate',1);"
http "사용중 client 삭제 409" 409 -H "$AUTH" -X DELETE $BASE/api/admin/v1/clients/notegate
sqlok "client 소유 file 정리" "DELETE FROM files WHERE client_id='notegate';"
http "client 삭제(키·자격증명 cascade) 204" 204 -H "$AUTH" -X DELETE $BASE/api/admin/v1/clients/notegate
http "client 삭제 멱등 204" 204 -H "$AUTH" -X DELETE $BASE/api/admin/v1/clients/notegate
LEFT=$($PSQL "SELECT (SELECT count(*) FROM client_keys WHERE client_id='notegate')+(SELECT count(*) FROM s3_credentials WHERE client_id='notegate');" -t | tr -d ' \n')
if [ "$LEFT" = "0" ]; then ok; else bad "client 소유물 cascade 잔여 $LEFT"; fi
http "storage-b 삭제 204"   204 -H "$AUTH" -X DELETE $BASE/api/admin/v1/storages/minio-b
http "storage 삭제 멱등 204" 204 -H "$AUTH" -X DELETE $BASE/api/admin/v1/storages/minio-b
REMAIN=$($PSQL "SELECT (SELECT count(*) FROM storages)+(SELECT count(*) FROM clients)+(SELECT count(*) FROM client_keys)+(SELECT count(*) FROM s3_credentials)+(SELECT count(*) FROM files);" -t | tr -d ' \n')
if [ "$REMAIN" = "0" ]; then ok; else bad "종료 후 잔여 행 $REMAIN"; fi

echo ""
echo "결과: PASS=$PASS FAIL=$FAIL"
exit $FAIL
