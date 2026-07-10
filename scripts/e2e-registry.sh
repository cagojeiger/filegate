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
PSQL="docker exec filegate-postgres-1 psql -U filegate -d filegate -qc"
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

# 시작 전 등록부 초기화 (문장별 개별 실행 — 순서: 엣지 → 노드)
$PSQL "DELETE FROM files;" >/dev/null 2>&1
$PSQL "DELETE FROM bindings;" >/dev/null 2>&1
$PSQL "DELETE FROM clients;" >/dev/null 2>&1
$PSQL "DELETE FROM storages;" >/dev/null 2>&1

echo "=== A. DB 제약 프로브 ==="
sqlfail "storage 슬러그 대문자" "INSERT INTO storages (id,endpoint,public_endpoint,region,bucket,force_path_style,access_key,secret_key_ciphertext,secret_key_nonce,enc_key_id,capacity_bytes) VALUES ('Bad_ID','e','e','r','b',false,'ak',$CT,$NONCE,'v1',0);"
sqlfail "nonce 11바이트" "INSERT INTO storages (id,endpoint,public_endpoint,region,bucket,force_path_style,access_key,secret_key_ciphertext,secret_key_nonce,enc_key_id,capacity_bytes) VALUES ('s1','e','e','r','b',false,'ak',$CT,decode(repeat('00',11),'hex'),'v1',0);"
sqlfail "capacity 음수" "INSERT INTO storages (id,endpoint,public_endpoint,region,bucket,force_path_style,access_key,secret_key_ciphertext,secret_key_nonce,enc_key_id,capacity_bytes) VALUES ('s1','e','e','r','b',false,'ak',$CT,$NONCE,'v1',-1);"
sqlok   "storage 정상" "INSERT INTO storages (id,endpoint,public_endpoint,region,bucket,force_path_style,access_key,secret_key_ciphertext,secret_key_nonce,enc_key_id,capacity_bytes) VALUES ('s1','e','e','r','b',false,'ak',$CT,$NONCE,'v1',10);"
sqlfail "storage id 중복" "INSERT INTO storages (id,endpoint,public_endpoint,region,bucket,force_path_style,access_key,secret_key_ciphertext,secret_key_nonce,enc_key_id,capacity_bytes) VALUES ('s1','e','e','r','b',false,'ak',$CT,$NONCE,'v1',10);"
sqlok   "client 정상" "INSERT INTO clients (id) VALUES ('c1');"
sqlfail "client 슬러그 위반" "INSERT INTO clients (id) VALUES ('-bad');"
sqlfail "key 해시 형식 위반" "INSERT INTO client_keys (key_hash,client_id) VALUES ('sha256:zzz','c1');"
sqlok   "key 정상" "INSERT INTO client_keys (key_hash,client_id) VALUES ('$HASH_A','c1');"
sqlok   "둘째 client" "INSERT INTO clients (id) VALUES ('c2');"
sqlfail "key 해시 전역 중복(다른 client라도)" "INSERT INTO client_keys (key_hash,client_id) VALUES ('$HASH_A','c2');"
sqlfail "없는 client의 binding" "INSERT INTO bindings (client_id,intent,storage_id) VALUES ('ghost','i','s1');"
sqlfail "없는 storage의 binding" "INSERT INTO bindings (client_id,intent,storage_id) VALUES ('c1','i','ghost');"
sqlfail "intent 슬러그 위반" "INSERT INTO bindings (client_id,intent,storage_id) VALUES ('c1','Bad','s1');"
sqlok   "binding 정상" "INSERT INTO bindings (client_id,intent,storage_id) VALUES ('c1','att','s1');"
sqlfail "binding (client,intent) 중복" "INSERT INTO bindings (client_id,intent,storage_id) VALUES ('c1','att','s1');"
sqlfail "binding 남은 storage 삭제" "DELETE FROM storages WHERE id='s1';"
sqlfail "binding 남은 client 삭제" "DELETE FROM clients WHERE id='c1';"
sqlfail "미등록 client의 file" "INSERT INTO files (client_id,intent,declared_size) VALUES ('ghost','att',1);"
sqlok   "등록 client의 file" "INSERT INTO files (client_id,intent,declared_size) VALUES ('c1','att',1);"
sqlok   "binding 삭제" "DELETE FROM bindings WHERE client_id='c1';"
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
http "인증 없음 401"        401 $BASE/admin/storages
http "틀린 토큰 401"        401 -H "Authorization: Bearer nope" $BASE/admin/storages
http "storage 틀린시크릿 400" 400 -H "$AUTH" -H "$JSON" -X POST $BASE/admin/storages -d "{\"id\":\"minio-a\",$(echo $SBAD | cut -c2-)"
http "storage 생성 201"     201 -H "$AUTH" -H "$JSON" -X POST $BASE/admin/storages -d "{\"id\":\"minio-a\",$(echo $S | cut -c2-)"
http "storage 중복 409"     409 -H "$AUTH" -H "$JSON" -X POST $BASE/admin/storages -d "{\"id\":\"minio-a\",$(echo $S | cut -c2-)"
http "storage 나쁜슬러그 400" 400 -H "$AUTH" -H "$JSON" -X POST $BASE/admin/storages -d "{\"id\":\"Bad_ID\",$(echo $S | cut -c2-)"
http "storage 둘째 생성 201" 201 -H "$AUTH" -H "$JSON" -X POST $BASE/admin/storages -d "{\"id\":\"minio-b\",$(echo $S | cut -c2-)"
http "storage 조회 200"     200 -H "$AUTH" $BASE/admin/storages/minio-a
http "storage 없는 조회 404" 404 -H "$AUTH" $BASE/admin/storages/ghost
http "storage 갱신 200"     200 -H "$AUTH" -H "$JSON" -X PUT $BASE/admin/storages/minio-a -d "$S"
http "storage 없는 갱신 404" 404 -H "$AUTH" -H "$JSON" -X PUT $BASE/admin/storages/ghost -d "$S"
http "client 생성 201"      201 -H "$AUTH" -H "$JSON" -X POST $BASE/admin/clients -d '{"id":"notegate"}'
http "client 중복 409"      409 -H "$AUTH" -H "$JSON" -X POST $BASE/admin/clients -d '{"id":"notegate"}'
http "client 조회 200"      200 -H "$AUTH" $BASE/admin/clients/notegate
http "client 없는 조회 404" 404 -H "$AUTH" $BASE/admin/clients/ghost
http "key 등록 201"         201 -H "$AUTH" -H "$JSON" -X POST $BASE/admin/clients/notegate/keys -d "{\"key_hash\":\"$HASH_A\"}"
http "key 중복 409"         409 -H "$AUTH" -H "$JSON" -X POST $BASE/admin/clients/notegate/keys -d "{\"key_hash\":\"$HASH_A\"}"
http "key 형식위반 400"     400 -H "$AUTH" -H "$JSON" -X POST $BASE/admin/clients/notegate/keys -d '{"key_hash":"sha256:short"}'
http "key 없는client 404"   404 -H "$AUTH" -H "$JSON" -X POST $BASE/admin/clients/ghost/keys -d "{\"key_hash\":\"$HASH_B\"}"
http "key 회전: 둘째 201"   201 -H "$AUTH" -H "$JSON" -X POST $BASE/admin/clients/notegate/keys -d "{\"key_hash\":\"$HASH_B\"}"
http "key 조회 200"         200 -H "$AUTH" $BASE/admin/clients/notegate/keys/$HASH_A
http "key 첫째 삭제 204"    204 -H "$AUTH" -X DELETE $BASE/admin/clients/notegate/keys/$HASH_A
http "key 삭제 멱등 204"    204 -H "$AUTH" -X DELETE $BASE/admin/clients/notegate/keys/$HASH_A
http "key 삭제후 조회 404"  404 -H "$AUTH" $BASE/admin/clients/notegate/keys/$HASH_A
http "binding 생성(POST) 201" 201 -H "$AUTH" -H "$JSON" -X POST $BASE/admin/clients/notegate/bindings/att -d '{"storage_id":"minio-a"}'
http "binding 중복 생성 409" 409 -H "$AUTH" -H "$JSON" -X POST $BASE/admin/clients/notegate/bindings/att -d '{"storage_id":"minio-b"}'
http "binding 조회 200"     200 -H "$AUTH" $BASE/admin/clients/notegate/bindings/att
http "binding 없는조회 404" 404 -H "$AUTH" $BASE/admin/clients/notegate/bindings/ghost
http "binding 재지정(PUT) 200" 200 -H "$AUTH" -H "$JSON" -X PUT $BASE/admin/clients/notegate/bindings/att -d '{"storage_id":"minio-b"}'
http "binding 없는것 갱신 404" 404 -H "$AUTH" -H "$JSON" -X PUT $BASE/admin/clients/notegate/bindings/ghost -d '{"storage_id":"minio-a"}'
MOVED=$(curl -s -H "$AUTH" $BASE/admin/clients/notegate/bindings/att | grep -c 'minio-b')
if [ "$MOVED" = "1" ]; then ok; else bad "binding 재지정 반영 안 됨"; fi
http "binding 없는storage 404" 404 -H "$AUTH" -H "$JSON" -X POST $BASE/admin/clients/notegate/bindings/att2 -d '{"storage_id":"ghost"}'
http "소문자 bearer 허용 200" 200 -H "authorization: bearer fgop_local-dev" $BASE/admin/storages
http "capacity 음수 400(네트워크 검증 전)" 400 -H "$AUTH" -H "$JSON" -X POST $BASE/admin/storages -d '{"id":"neg","endpoint":"http://127.0.0.1:1","region":"r","bucket":"b","access_key":"a","secret_key":"s","capacity_bytes":-1}'
http "없는 storage 갱신 404(네트워크 검증 전)" 404 -H "$AUTH" -H "$JSON" -X PUT $BASE/admin/storages/ghost2 -d '{"endpoint":"http://127.0.0.1:1","region":"r","bucket":"b","access_key":"a","secret_key":"s","capacity_bytes":1}'
http "사용중 storage-b 삭제 409" 409 -H "$AUTH" -X DELETE $BASE/admin/storages/minio-b
http "미사용 storage-a 삭제 204" 204 -H "$AUTH" -X DELETE $BASE/admin/storages/minio-a
http "사용중 client 삭제 409" 409 -H "$AUTH" -X DELETE $BASE/admin/clients/notegate
http "binding 삭제 204"     204 -H "$AUTH" -X DELETE $BASE/admin/clients/notegate/bindings/att
http "binding 삭제 멱등 204" 204 -H "$AUTH" -X DELETE $BASE/admin/clients/notegate/bindings/att
http "client 삭제(key cascade) 204" 204 -H "$AUTH" -X DELETE $BASE/admin/clients/notegate
http "client 삭제 멱등 204" 204 -H "$AUTH" -X DELETE $BASE/admin/clients/notegate
http "storage-b 삭제 204"   204 -H "$AUTH" -X DELETE $BASE/admin/storages/minio-b
http "storage 삭제 멱등 204" 204 -H "$AUTH" -X DELETE $BASE/admin/storages/minio-b
REMAIN=$($PSQL "SELECT (SELECT count(*) FROM storages)+(SELECT count(*) FROM clients)+(SELECT count(*) FROM client_keys)+(SELECT count(*) FROM bindings);" -t | tr -d ' \n')
if [ "$REMAIN" = "0" ]; then ok; else bad "종료 후 잔여 행 $REMAIN"; fi

echo ""
echo "결과: PASS=$PASS FAIL=$FAIL"
exit $FAIL
