# 로컬 등록 그래프 — published 프로바이더(cagojeiger/filegate)로 등록부 전체를
# 한 번의 apply로 세운다. e2e 스크립트(scripts/e2e-*.sh)의 전제 상태다.
#
#   storage(minio) ◀── binding(attachment) ── client(notegate) ── client_key
#
# 실행:
#   docker compose up -d && cargo run --bin filegate   # 서버 기동
#   export FILEGATE_OPERATOR_TOKEN=fgop_local-dev
#   terraform -chdir=deploy/local init
#   terraform -chdir=deploy/local apply
#
# docker-compose.yml의 MinIO 자격증명과 동일한 로컬 개발 값이다. 실전은 TF 변수·시크릿.

terraform {
  required_providers {
    filegate = {
      source  = "cagojeiger/filegate"
      version = "0.1.0"
    }
  }
}

provider "filegate" {
  endpoint = "http://127.0.0.1:8080"
  # token은 env FILEGATE_OPERATOR_TOKEN으로 공급한다.
}

# ── storage: 물리 저장 공간 (독립 노드) ──────────────────────────

resource "filegate_storage_s3" "minio_local" {
  id              = "minio-local"
  endpoint        = "http://127.0.0.1:9000"
  public_endpoint = "http://127.0.0.1:9000"
  region          = "us-east-1"
  bucket          = "filegate-std"

  force_path_style = true

  access_key     = "filegate"
  secret_key     = "filegate-secret"
  capacity_bytes = 1073741824 # 1 GiB
}

# 중계(relay) s3: 같은 MinIO를 filegate 바이트 엔드포인트로 강제.
# 서버에 FILEGATE_PUBLIC_URL이 서 있어야 등록된다.
resource "filegate_storage_s3" "minio_relay" {
  id               = "minio-relay"
  endpoint         = "http://127.0.0.1:9000"
  region           = "us-east-1"
  bucket           = "filegate-std"
  force_path_style = true
  force_relay      = true

  access_key     = "filegate"
  secret_key     = "filegate-secret"
  capacity_bytes = 1073741824
}

resource "filegate_storage_fs" "local_fs" {
  id             = "fs-local"
  root_path      = "/tmp/filegate-fs-demo"
  capacity_bytes = 1073741824
}

# ── client: 서비스 신원 (독립 노드) ──────────────────────────────

resource "filegate_client" "notegate" {
  id = "notegate"
}

# raw 키는 여기(TF state)에만 존재한다 — filegate에는 해시만 등록된다.
locals {
  notegate_raw_key = "fg_local-dev-notegate-key-0123456789abcdef"
}

resource "filegate_client_key" "notegate" {
  client_id = filegate_client.notegate.id
  key_hash  = "sha256:${sha256(local.notegate_raw_key)}"
}

# ── binding: 두 노드를 잇는 엣지 ─────────────────────────────────

resource "filegate_binding" "notegate_attachment" {
  client_id  = filegate_client.notegate.id
  intent     = "attachment"
  storage_id = filegate_storage_s3.minio_local.id
}

resource "filegate_binding" "notegate_relay_att" {
  client_id  = filegate_client.notegate.id
  intent     = "relay-att"
  storage_id = filegate_storage_s3.minio_relay.id
}

resource "filegate_binding" "notegate_fs_att" {
  client_id  = filegate_client.notegate.id
  intent     = "fs-att"
  storage_id = filegate_storage_fs.local_fs.id
}
