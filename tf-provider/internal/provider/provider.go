package provider

import (
	"context"
	"os"

	"github.com/hashicorp/terraform-plugin-framework/datasource"
	"github.com/hashicorp/terraform-plugin-framework/provider"
	"github.com/hashicorp/terraform-plugin-framework/provider/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/types"
)

// filegateProvider는 provider 블록(연결 정보)만 다룬다. 리소스가 등록부다.
type filegateProvider struct{}

func New() provider.Provider {
	return &filegateProvider{}
}

type providerModel struct {
	Endpoint types.String `tfsdk:"endpoint"`
	Token    types.String `tfsdk:"token"`
}

func (p *filegateProvider) Metadata(
	_ context.Context,
	_ provider.MetadataRequest,
	response *provider.MetadataResponse,
) {
	response.TypeName = "filegate"
}

func (p *filegateProvider) Schema(
	_ context.Context,
	_ provider.SchemaRequest,
	response *provider.SchemaResponse,
) {
	response.Schema = schema.Schema{
		Description: "filegate 운영자 API 클라이언트 (등록부의 정본은 filegate DB).",
		Attributes: map[string]schema.Attribute{
			"endpoint": schema.StringAttribute{
				Optional:    true,
				Description: "filegate 주소. 생략 시 env FILEGATE_ENDPOINT.",
			},
			"token": schema.StringAttribute{
				Optional:    true,
				Sensitive:   true,
				Description: "운영자 토큰. 생략 시 env FILEGATE_OPERATOR_TOKEN.",
			},
		},
	}
}

func (p *filegateProvider) Configure(
	ctx context.Context,
	request provider.ConfigureRequest,
	response *provider.ConfigureResponse,
) {
	var config providerModel
	response.Diagnostics.Append(request.Config.Get(ctx, &config)...)
	if response.Diagnostics.HasError() {
		return
	}

	endpoint := config.Endpoint.ValueString()
	if endpoint == "" {
		endpoint = os.Getenv("FILEGATE_ENDPOINT")
	}
	token := config.Token.ValueString()
	if token == "" {
		token = os.Getenv("FILEGATE_OPERATOR_TOKEN")
	}
	if endpoint == "" {
		response.Diagnostics.AddError(
			"missing endpoint",
			"provider 블록의 endpoint 또는 env FILEGATE_ENDPOINT가 필요하다.",
		)
	}
	if token == "" {
		response.Diagnostics.AddError(
			"missing operator token",
			"provider 블록의 token 또는 env FILEGATE_OPERATOR_TOKEN이 필요하다.",
		)
	}
	if response.Diagnostics.HasError() {
		return
	}

	client := newAPIClient(endpoint, token)
	response.ResourceData = client
}

func (p *filegateProvider) Resources(_ context.Context) []func() resource.Resource {
	return []func() resource.Resource{
		NewStorageResource,
		NewStorageFsResource,
		NewClientResource,
		NewClientKeyResource,
		NewBindingResource,
	}
}

func (p *filegateProvider) DataSources(_ context.Context) []func() datasource.DataSource {
	return nil
}
