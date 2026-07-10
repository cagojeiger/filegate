# 로컬 hello-world: docker-compose의 MinIO를 filegate에 등록한다.
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

resource "filegate_provider" "minio_local" {
  id               = "minio-local"
  endpoint         = "http://127.0.0.1:9000"
  region           = "us-east-1"
  bucket           = "filegate-std"
  force_path_style = true

  # 로컬 개발 전용 값 (docker-compose.yml과 동일). 실전은 TF 변수·시크릿으로.
  access_key     = "filegate"
  secret_key     = "filegate-secret"
  capacity_bytes = 1073741824 # 1 GiB
}
