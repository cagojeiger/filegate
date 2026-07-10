package provider

import (
	"context"
	"fmt"
	"net/http"

	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/types"
)

// filegate_client — 서비스 신원 등록 (spec 01). storage와 독립인 노드다.
// binding이 남아 있으면 destroy가 409로 거부된다 — TF 의존 순서가 지켜준다.
type clientResource struct {
	client *apiClient
}

func NewClientResource() resource.Resource {
	return &clientResource{}
}

type clientResourceModel struct {
	ID types.String `tfsdk:"id"`
}

func (r *clientResource) Metadata(
	_ context.Context,
	request resource.MetadataRequest,
	response *resource.MetadataResponse,
) {
	response.TypeName = request.ProviderTypeName + "_client"
}

func (r *clientResource) Schema(
	_ context.Context,
	_ resource.SchemaRequest,
	response *resource.SchemaResponse,
) {
	response.Schema = schema.Schema{
		Description: "filegate를 쓰는 서비스의 등록 단위. 키와 intent 이름의 네임스페이스다.",
		Attributes: map[string]schema.Attribute{
			"id": schema.StringAttribute{
				Required:    true,
				Description: "안정 슬러그 (생성 후 불변 — 바꾸면 재생성).",
				PlanModifiers: []planmodifier.String{
					stringplanmodifier.RequiresReplace(),
				},
			},
		},
	}
}

func (r *clientResource) Configure(
	_ context.Context,
	request resource.ConfigureRequest,
	response *resource.ConfigureResponse,
) {
	if request.ProviderData == nil {
		return
	}
	client, ok := request.ProviderData.(*apiClient)
	if !ok {
		response.Diagnostics.AddError(
			"unexpected provider data",
			fmt.Sprintf("expected *apiClient, got %T", request.ProviderData),
		)
		return
	}
	r.client = client
}

func (r *clientResource) Create(
	ctx context.Context,
	request resource.CreateRequest,
	response *resource.CreateResponse,
) {
	var plan clientResourceModel
	response.Diagnostics.Append(request.Plan.Get(ctx, &plan)...)
	if response.Diagnostics.HasError() {
		return
	}

	body := map[string]string{"id": plan.ID.ValueString()}
	if _, err := r.client.do(ctx, http.MethodPost, "/admin/clients", body, nil); err != nil {
		response.Diagnostics.AddError("client registration failed", err.Error())
		return
	}
	response.Diagnostics.Append(response.State.Set(ctx, plan)...)
}

func (r *clientResource) Read(
	ctx context.Context,
	request resource.ReadRequest,
	response *resource.ReadResponse,
) {
	var state clientResourceModel
	response.Diagnostics.Append(request.State.Get(ctx, &state)...)
	if response.Diagnostics.HasError() {
		return
	}

	status, err := r.client.do(
		ctx, http.MethodGet, "/admin/clients/"+state.ID.ValueString(), nil, nil,
	)
	if status == http.StatusNotFound {
		response.State.RemoveResource(ctx)
		return
	}
	if err != nil {
		response.Diagnostics.AddError("client read failed", err.Error())
	}
}

// id뿐인 리소스라 갱신할 것이 없다 — id 변경은 RequiresReplace가 재생성으로 푼다.
func (r *clientResource) Update(
	_ context.Context,
	_ resource.UpdateRequest,
	response *resource.UpdateResponse,
) {
	response.Diagnostics.AddError(
		"unreachable update",
		"filegate_client has no updatable attribute",
	)
}

func (r *clientResource) Delete(
	ctx context.Context,
	request resource.DeleteRequest,
	response *resource.DeleteResponse,
) {
	var state clientResourceModel
	response.Diagnostics.Append(request.State.Get(ctx, &state)...)
	if response.Diagnostics.HasError() {
		return
	}

	path := "/admin/clients/" + state.ID.ValueString()
	if _, err := r.client.do(ctx, http.MethodDelete, path, nil, nil); err != nil {
		// binding·file이 남아 있으면 filegate가 409로 거부한다.
		response.Diagnostics.AddError("client delete failed", err.Error())
	}
}
