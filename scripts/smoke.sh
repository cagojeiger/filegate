#!/usr/bin/env bash
# End-to-end smoke test against a locally running filegate + docker-compose stack.
# Exercises the full lease lifecycle: issue -> direct PUT -> commit -> read -> GET -> detach.
set -euo pipefail

BASE="${FILEGATE_BASE:-http://127.0.0.1:8086}"
KEY="${FILEGATE_CLIENT_NOTEGATE_KEY:-dev-notegate-key}"
AUTH="Authorization: Bearer ${KEY}"

payload="hello filegate $(date +%s)"
size=$(printf %s "$payload" | wc -c | tr -d ' ')

echo "== healthz"
curl -fsS "$BASE/healthz"; echo

echo "== 1. issue write lease (intent: note_attachment, size: $size)"
create=$(curl -fsS -X POST "$BASE/v1/files" -H "$AUTH" -H 'Content-Type: application/json' \
  -d "{\"intent\":\"note_attachment\",\"size\":$size,\"content_type\":\"text/plain\",\"metadata\":{\"filename\":\"hello.txt\"}}")
echo "$create"
file_id=$(echo "$create" | python3 -c 'import json,sys; print(json.load(sys.stdin)["file_id"])')
lease_id=$(echo "$create" | python3 -c 'import json,sys; print(json.load(sys.stdin)["upload"]["lease_id"])')
put_url=$(echo "$create" | python3 -c 'import json,sys; print(json.load(sys.stdin)["upload"]["url"])')

echo "== 2. upload bytes directly to storage (filegate never sees them)"
curl -fsS -X PUT "$put_url" -H 'Content-Type: text/plain' --data-binary "$payload" -o /dev/null
echo "uploaded"

echo "== 3. commit (verification gate)"
curl -fsS -X POST "$BASE/v1/leases/$lease_id/commit" -H "$AUTH"; echo

echo "== 4. file metadata (no placement info should appear)"
curl -fsS "$BASE/v1/files/$file_id" -H "$AUTH"; echo

echo "== 5. issue read lease"
read_lease=$(curl -fsS -X POST "$BASE/v1/files/$file_id/leases" -H "$AUTH")
echo "$read_lease"
get_url=$(echo "$read_lease" | python3 -c 'import json,sys; print(json.load(sys.stdin)["url"])')

echo "== 6. download directly from storage and compare"
downloaded=$(curl -fsS "$get_url")
if [ "$downloaded" = "$payload" ]; then
  echo "roundtrip OK: '$downloaded'"
else
  echo "MISMATCH: expected '$payload', got '$downloaded'"; exit 1
fi

echo "== 7. usage"
curl -fsS "$BASE/v1/usage" -H "$AUTH"; echo

echo "== 8. detach (delete decision)"
curl -fsS -X DELETE "$BASE/v1/files/$file_id" -H "$AUTH" -o /dev/null -w "%{http_code}\n"

echo "== 9. read lease after detach should fail (409)"
code=$(curl -s -o /dev/null -w "%{http_code}" -X POST "$BASE/v1/files/$file_id/leases" -H "$AUTH")
if [ "$code" = "409" ]; then echo "correctly refused: $code"; else echo "unexpected: $code"; exit 1; fi

echo
echo "smoke test passed ✓"
