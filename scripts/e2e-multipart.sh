#!/bin/sh
# multipart 동등성 E2E (spec 02 완료 조건): 같은 대용량 시나리오가
# 직결(minio) = 중계(minio) = 중계(fs)에서 같은 상태 전이·회계·응답을 낸다.
# + 강화 케이스: 재발급(재개), part 크기·범위 검증, 미완성 commit 400,
#   미완성 회수(벤더 Abort·fs mp 임시 삭제), purge 후 소멸.
#
# 전제: 서버가 작은 multipart 설정으로 실행 중이어야 한다:
#   FILEGATE_MULTIPART_THRESHOLD_BYTES=6291456 (6MiB)
#   FILEGATE_PART_SIZE_BYTES=5242880 (5MiB)
#   (+ FILEGATE_PUBLIC_URL, 짧은 reconciler tick)
# 12MiB 파일 → part 3개 (5MiB, 5MiB, 2MiB). 사용: sh scripts/e2e-multipart.sh
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
md5file() { md5 -q "$1" 2>/dev/null || md5sum "$1" | cut -d' ' -f1; }

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

# 시작 전 도메인 초기화
$PSQL "DELETE FROM lease_parts;" >/dev/null 2>&1
$PSQL "DELETE FROM leases;" >/dev/null 2>&1
$PSQL "DELETE FROM locations;" >/dev/null 2>&1
$PSQL "DELETE FROM files;" >/dev/null 2>&1
mkdir -p /tmp/filegate-fs-demo
rm -rf /tmp/filegate-fs-demo/fg /tmp/filegate-fs-demo/.fg-tmp-* 2>/dev/null
fs_count() { find /tmp/filegate-fs-demo -type f ! -name '.fg-tmp-*' | wc -l | tr -d ' '; }

# 12MiB 페이로드와 part 분할 (part 크기 5MiB)
dd if=/dev/urandom of="$WORK/big.bin" bs=1048576 count=12 2>/dev/null
dd if="$WORK/big.bin" of="$WORK/p1" bs=1048576 count=5 2>/dev/null
dd if="$WORK/big.bin" of="$WORK/p2" bs=1048576 count=5 skip=5 2>/dev/null
dd if="$WORK/big.bin" of="$WORK/p3" bs=1048576 count=2 skip=10 2>/dev/null
WHOLE_MD5=$(md5file "$WORK/big.bin")
SIZE=12582912

# 발급된 parts 응답에서 n번 part의 URL을 뽑는다.
part_url() { printf '%s' "$1" | python3 -c "import sys,json; parts=json.load(sys.stdin)['parts']; print(next(p['url'] for p in parts if p['part']==$2))" 2>/dev/null; }

# ── 공통 시나리오 ── $1 intent, $2 storage_id, $3 direct|relay
run_mode() {
  INTENT=$1; SID=$2; URLKIND=$3
  echo "--- [multipart $INTENT → $SID / $URLKIND]"
  C=$(curl -s -H "$AUTH" -H "$JSON" -X POST $BASE/v1/files \
    -d "{\"intent\":\"$INTENT\",\"declared_size\":$SIZE,\"content_type\":\"application/zip\"}")
  FID=$(printf '%s' "$C" | sed -n 's/.*"file_id":"\([^"]*\)".*/\1/p')
  if [ -n "$FID" ]; then ok; else bad "[$INTENT] create 실패: $C"; return; fi
  case "$C" in *'"put_url"'*) bad "[$INTENT] multipart인데 put_url이 있음: $C";; *) ok;; esac
  case "$C" in *'"part_size":5242880'*'"part_count":3'*) ok;; *) bad "[$INTENT] 서술자 불일치: $C";; esac

  P=$(curl -s -H "$AUTH" -H "$JSON" -X POST $BASE/v1/files/$FID/parts -d '{"parts":[1,2,3]}')
  U1=$(part_url "$P" 1); U2=$(part_url "$P" 2); U3=$(part_url "$P" 3)
  if [ -n "$U1" ] && [ -n "$U2" ] && [ -n "$U3" ]; then ok; else bad "[$INTENT] parts 발급 실패: $P"; return; fi
  case "$URLKIND" in
    direct) case "$U1" in http://127.0.0.1:9000/*) ok;; *) bad "[$INTENT] 직결 part URL 아님: $U1";; esac;;
    relay)  case "$U1" in $BASE/b/*part=1*|$BASE/b/*\?s=*) ok;; *) bad "[$INTENT] 중계 part URL 아님: $U1";; esac;;
  esac

  expect "[$INTENT] part1 PUT 200" 200 "$(curl -s -o /dev/null -w '%{http_code}' -X PUT --data-binary @"$WORK/p1" "$U1")"
  expect "[$INTENT] part3 PUT 200 (순서 무관)" 200 "$(curl -s -o /dev/null -w '%{http_code}' -X PUT --data-binary @"$WORK/p3" "$U3")"
  # 미완성 commit → 400, pending 유지 (part 2 없음)
  expect "[$INTENT] 미완성 commit 400" 400 "$(curl -s -o /dev/null -w '%{http_code}' -H "$AUTH" -X POST $BASE/v1/files/$FID/commit)"
  # 크기 불일치 part의 즉시 차단은 중계만 가능 — 직결은 벤더가 크기를 안
  # 막고 commit이 사후 게이트다 (단일 PUT과 같은 경계, spec 00).
  if [ "$URLKIND" = "relay" ]; then
    expect "[$INTENT] part 크기 불일치 400" 400 "$(curl -s -o /dev/null -w '%{http_code}' -X PUT --data-binary @"$WORK/p3" "$U2")"
  fi
  # 재발급 = 재개 (spec 02): part2를 다시 요청해 올린다. 중계는 시크릿이
  # 회전하지 않아야 하므로, 재발급 뒤에도 앞서 받은 part1 URL(U1)이 살아
  # 있어야 한다 — 재개가 진행 중 다른 part를 죽이지 않는다는 계약.
  P2=$(curl -s -H "$AUTH" -H "$JSON" -X POST $BASE/v1/files/$FID/parts -d '{"parts":[2]}')
  U2B=$(part_url "$P2" 2)
  expect "[$INTENT] 재발급 part2 PUT 200" 200 "$(curl -s -o /dev/null -w '%{http_code}' -X PUT --data-binary @"$WORK/p2" "$U2B")"
  if [ "$URLKIND" = "relay" ]; then
    # 앞 배치의 part1 URL을 재사용해 다시 PUT — 시크릿 비회전이면 200.
    expect "[$INTENT] 재발급 후 앞 배치 URL 생존(비회전)" 200 "$(curl -s -o /dev/null -w '%{http_code}' -X PUT --data-binary @"$WORK/p1" "$U1")"
  fi

  CM=$(curl -s -w '\n%{http_code}' -H "$AUTH" -X POST $BASE/v1/files/$FID/commit)
  expect "[$INTENT] commit 200" 200 "$(printf '%s' "$CM" | tail -1)"
  case "$CM" in *'-3'*) ok;; *) bad "[$INTENT] multipart ETag(-3) 아님: $CM";; esac
  expect "[$INTENT] 회계 active 12MiB" "$SIZE" "$($PSQL "SELECT coalesce(sum(f.declared_size),0) FROM files f JOIN locations l ON l.file_id=f.id WHERE l.storage_id='$SID' AND f.state='active';" | tr -d ' ')"

  R=$(curl -s -H "$AUTH" -H "$JSON" -X POST $BASE/v1/files/$FID/read -d '{}')
  GURL=$(printf '%s' "$R" | sed -n 's/.*"get_url":"\([^"]*\)".*/\1/p')
  curl -s -o "$WORK/down.bin" "$GURL"
  expect "[$INTENT] 다운로드 md5 일치" "$WHOLE_MD5" "$(md5file "$WORK/down.bin")"
  eval "FID_$(printf '%s' "$INTENT" | tr - _)=$FID"
}

echo "=== 3-모드 동등성 (multipart) ==="
run_mode attachment minio-local  direct
run_mode relay-att  minio-relay  relay
run_mode fs-att     fs-local     relay

echo "=== part 검증 강화 ==="
C=$(curl -s -H "$AUTH" -H "$JSON" -X POST $BASE/v1/files -d "{\"intent\":\"relay-att\",\"declared_size\":$SIZE}")
FIDX=$(printf '%s' "$C" | sed -n 's/.*"file_id":"\([^"]*\)".*/\1/p')
# (curl을 expect 인자 안에 중첩하면 macOS bash 3.2의 인용 버그로 JSON이 깨진다)
MD5_REJECT=$(curl -s -o /dev/null -w '%{http_code}' -H "$AUTH" -H "$JSON" -X POST $BASE/v1/files \
  -d "{\"intent\":\"relay-att\",\"declared_size\":$SIZE,\"declared_md5\":\"$WHOLE_MD5\"}")
expect "multipart create에 declared_md5 400" 400 "$MD5_REJECT"
expect "part 범위 초과 400" 400 "$(curl -s -o /dev/null -w '%{http_code}' -H "$AUTH" -H "$JSON" -X POST $BASE/v1/files/$FIDX/parts -d '{"parts":[4]}')"
P=$(curl -s -H "$AUTH" -H "$JSON" -X POST $BASE/v1/files/$FIDX/parts -d '{"parts":[1]}')
UX=$(part_url "$P" 1)
# part 파라미터 없이 multipart lease에 PUT → 400
UX_NOPART=$(printf '%s' "$UX" | sed 's/&part=1//')
expect "part 파라미터 없는 PUT 400" 400 "$(curl -s -o /dev/null -w '%{http_code}' -X PUT --data-binary @"$WORK/p1" "$UX_NOPART")"
printf '' | curl -s -o /dev/null -X PUT --data-binary @"$WORK/p1" "$UX"  # part1만 올려두고
echo "=== 미완성 회수: 만료 → Abort + 잔여물 소멸 + 회계 0 ==="
$PSQL "UPDATE leases SET expires_at = now() - interval '1 second' WHERE file_id='$FIDX' AND kind='write';" >/dev/null
sleep 6
expect "미완성 파일 reclaimed" "reclaimed" "$($PSQL "SELECT state FROM files WHERE id='$FIDX';" | tr -d ' ')"
# 벤더에 미완성 multipart 세션이 남지 않았는지 (mc ls --incomplete)
INCOMPLETE=$(docker run --rm --network host --entrypoint sh minio/mc:RELEASE.2025-08-13T08-35-41Z -c \
  "mc alias set m http://127.0.0.1:9000 filegate filegate-secret >/dev/null 2>&1 && mc ls --incomplete --recursive m/filegate-std 2>/dev/null | wc -l" | tr -d ' ')
expect "벤더 미완성 세션 0 (Abort 확인)" 0 "$INCOMPLETE"

echo "=== 세 모드 delete → purge → 회계 0 + fs 소멸 ==="
for I in attachment relay_att fs_att; do
  FID=$(eval echo "\$FID_$I")
  expect "[$I] delete 200" 200 "$(curl -s -o /dev/null -w '%{http_code}' -H "$AUTH" -X DELETE $BASE/v1/files/$FID)"
done
sleep 8
expect "회계 전부 0" "0" "$($PSQL "SELECT coalesce(sum(f.declared_size),0) FROM files f JOIN locations l ON l.file_id=f.id;" | tr -d ' ')"
expect "fs 실물 전부 소멸" 0 "$(fs_count)"
FS_MP=$(find /tmp/filegate-fs-demo -name '.fg-tmp-mp-*' | wc -l | tr -d ' ')
expect "fs multipart 임시 소멸" 0 "$FS_MP"

# 정리
$PSQL "DELETE FROM lease_parts;" >/dev/null 2>&1
$PSQL "DELETE FROM leases;" >/dev/null 2>&1
$PSQL "DELETE FROM locations;" >/dev/null 2>&1
$PSQL "DELETE FROM files;" >/dev/null 2>&1

echo ""
echo "결과: PASS=$PASS FAIL=$FAIL"
exit $FAIL
