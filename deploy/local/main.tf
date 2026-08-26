# 로컬 등록 그래프 — published 프로바이더(cagojeiger/filegate)로 등록부 전체를
# 한 번의 apply로 세운다. e2e 스크립트(scripts/e2e-*.sh)의 전제 상태다.
#
#   storage ◀── client ── client_key
#
# 실행:
#   docker compose up -d && cargo run --bin filegate   # 서버 기동
#   mkdir -p /tmp/filegate-fs-demo
#   export FILEGATE_OPERATOR_TOKEN=fgop_local-dev
#   terraform -chdir=deploy/local init -upgrade
#   terraform -chdir=deploy/local apply
#
# v0.1.0 로컬 state에서 올릴 때는 제거된 binding 주소를 state에서 한 번만 뺀다.
# 이 명령은 DB 리소스를 삭제하지 않는다.
#   terraform -chdir=deploy/local state rm \
#     filegate_binding.notegate_attachment \
#     filegate_binding.notegate_relay_att \
#     filegate_binding.notegate_fs_att
#
# docker-compose.yml의 MinIO 자격증명과 동일한 로컬 개발 값이다. 실전은 TF 변수·시크릿.

terraform {
  required_providers {
    filegate = {
      source  = "cagojeiger/filegate"
      version = "0.3.0"
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

# ── client: 서비스 신원 + 단일 기반 storage ─────────────────────

resource "filegate_client" "notegate" {
  id         = "notegate"
  storage_id = filegate_storage_s3.minio_local.id
}

resource "filegate_client" "notegate_relay" {
  id         = "notegate-relay"
  storage_id = filegate_storage_s3.minio_relay.id
}

resource "filegate_client" "notegate_fs" {
  id         = "notegate-fs"
  storage_id = filegate_storage_fs.local_fs.id
}

# raw 키는 여기(TF state)에만 존재한다 — filegate에는 해시만 등록된다.
locals {
  notegate_raw_key       = "fg_local-dev-notegate-key-0123456789abcdef"
  notegate_relay_raw_key = "fg_local-dev-notegate-relay-key-0123456789abcdef"
  notegate_fs_raw_key    = "fg_local-dev-notegate-fs-key-0123456789abcdef"
}

resource "filegate_client_key" "notegate" {
  client_id = filegate_client.notegate.id
  key_hash  = "sha256:${sha256(local.notegate_raw_key)}"
}

resource "filegate_client_key" "notegate_relay" {
  client_id = filegate_client.notegate_relay.id
  key_hash  = "sha256:${sha256(local.notegate_relay_raw_key)}"
}

resource "filegate_client_key" "notegate_fs" {
  client_id = filegate_client.notegate_fs.id
  key_hash  = "sha256:${sha256(local.notegate_fs_raw_key)}"
}
