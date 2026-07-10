// terraform-provider-filegate — filegate 운영자 API의 번역기 (spec 01).
// 등록부의 정본은 filegate DB다. 이 provider는 선언(.tf)과 등록부를
// 맞추는 얇은 클라이언트일 뿐, 자체 상태를 갖지 않는다.
package main

import (
	"context"
	"log"

	"github.com/hashicorp/terraform-plugin-framework/providerserver"

	"github.com/cagojeiger/terraform-provider-filegate/internal/provider"
)

func main() {
	err := providerserver.Serve(context.Background(), provider.New, providerserver.ServeOpts{
		Address: "registry.terraform.io/cagojeiger/filegate",
	})
	if err != nil {
		log.Fatal(err)
	}
}
