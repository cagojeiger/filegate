# 로컬 hello-world: 등록부 전체 그래프를 한 번의 apply로 세운다.
#
#   storage(minio) ◀── binding(attachment) ── client(notegate) ── client_key
#
# 실행 (dev_overrides — 레지스트리 없이 로컬 빌드 사용):
#   cd tf-provider && go build -o bin/terraform-provider-filegate .
#   export TF_CLI_CONFIG_FILE=$PWD/dev.tfrc   # dev.tfrc.example 참고
#   export FILEGATE_OPERATOR_TOKEN=fgop_local-dev
#   terraform -chdir=examples apply
#
# dev_overrides 모드에서는 terraform init이 필요 없다 (경고는 정상).

terraform {
  required_providers {
    filegate = {
      source = "cagojeiger/filegate"
    }
  }
}

provider "filegate" {
  endpoint = "http://127.0.0.1:8080"
  # token은 env FILEGATE_OPERATOR_TOKEN으로 공급한다.
}

# ── storage: 물리 저장 공간 (독립 노드) ──────────────────────────

resource "filegate_storage" "minio_local" {
  id              = "minio-local"
  endpoint        = "http://127.0.0.1:9000"
  public_endpoint = "http://127.0.0.1:9000"
  region          = "us-east-1"
  bucket          = "filegate-std"

  force_path_style = true

  # 로컬 개발 전용 값 (docker-compose.yml과 동일). 실전은 TF 변수·시크릿으로.
  access_key     = "filegate"
  secret_key     = "filegate-secret"
  capacity_bytes = 1073741824 # 1 GiB
}

# ── client: 서비스 신원 (독립 노드) ──────────────────────────────

resource "filegate_client" "notegate" {
  id = "notegate"
}

# raw 키는 여기(TF state)에만 존재한다 — filegate에는 해시만 등록된다.
# 실전은 random_password 리소스로 생성하고, raw는 대상 서비스의
# k8s Secret으로 배달한다 (spec 01).
locals {
  notegate_raw_key = "fg_local-dev-notegate-key-0123456789abcdef"
}

resource "filegate_client_key" "notegate" {
  client_id = filegate_client.notegate.id
  key_hash  = "sha256:${sha256(local.notegate_raw_key)}"
}

# ── 중계 모드 (relay): 같은 MinIO를 filegate 중계로, 그리고 로컬 fs ──
# 서버에 FILEGATE_PUBLIC_URL이 서 있어야 등록된다.

resource "filegate_storage" "minio_relay" {
  id               = "minio-relay"
  endpoint         = "http://127.0.0.1:9000"
  region           = "us-east-1"
  bucket           = "filegate-std"
  force_path_style = true
  force_relay      = true # 직결 대신 filegate 바이트 엔드포인트로

  access_key     = "filegate"
  secret_key     = "filegate-secret"
  capacity_bytes = 1073741824
}

resource "filegate_storage_fs" "local_fs" {
  id             = "fs-local"
  root_path      = "/tmp/filegate-fs-demo" # 로컬 개발용 — 실전은 NFS 마운트
  capacity_bytes = 1073741824
}

resource "filegate_binding" "notegate_relay_att" {
  client_id  = filegate_client.notegate.id
  intent     = "relay-att"
  storage_id = filegate_storage.minio_relay.id
}

resource "filegate_binding" "notegate_fs_att" {
  client_id  = filegate_client.notegate.id
  intent     = "fs-att"
  storage_id = filegate_storage_fs.local_fs.id
}

# ── binding: 두 노드를 잇는 엣지 ─────────────────────────────────
# storage_id 한 줄이 배치 선언이다. 바꾸면 새 파일만 새 곳으로 간다 (v0).

resource "filegate_binding" "notegate_attachment" {
  client_id  = filegate_client.notegate.id
  intent     = "attachment"
  storage_id = filegate_storage.minio_local.id
}
