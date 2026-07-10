package provider

import (
	"context"
	"fmt"
	"net/http"
	"net/url"

	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/types"
)

// filegate_client_key — 클라이언트 키의 sha256 해시 등록 (spec 01).
// raw 키는 생성자(TF state)에만 존재하고 filegate에 도달하지 않는다.
// 회전 = 행 추가·삭제이므로 모든 속성이 불변(재생성)이다.
type clientKeyResource struct {
	client *apiClient
}

func NewClientKeyResource() resource.Resource {
	return &clientKeyResource{}
}

type clientKeyResourceModel struct {
	ClientID types.String `tfsdk:"client_id"`
	KeyHash  types.String `tfsdk:"key_hash"`
}

func (r *clientKeyResource) Metadata(
	_ context.Context,
	request resource.MetadataRequest,
	response *resource.MetadataResponse,
) {
	response.TypeName = request.ProviderTypeName + "_client_key"
}

func (r *clientKeyResource) Schema(
	_ context.Context,
	_ resource.SchemaRequest,
	response *resource.SchemaResponse,
) {
	replace := []planmodifier.String{stringplanmodifier.RequiresReplace()}
	response.Schema = schema.Schema{
		Description: "클라이언트 인증 키의 해시 등록. raw 키는 filegate에 저장되지 않는다 — " +
			"회전은 키 추가·삭제로 한다.",
		Attributes: map[string]schema.Attribute{
			"client_id": schema.StringAttribute{
				Required:      true,
				PlanModifiers: replace,
			},
			"key_hash": schema.StringAttribute{
				Required:      true,
				Description:   "sha256:<64hex>. 예: \"sha256:${sha256(var.raw_key)}\".",
				PlanModifiers: replace,
			},
		},
	}
}

func (r *clientKeyResource) Configure(
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

func (r *clientKeyResource) keyPath(model clientKeyResourceModel) string {
	return "/admin/clients/" + url.PathEscape(model.ClientID.ValueString()) +
		"/keys/" + url.PathEscape(model.KeyHash.ValueString())
}

func (r *clientKeyResource) Create(
	ctx context.Context,
	request resource.CreateRequest,
	response *resource.CreateResponse,
) {
	var plan clientKeyResourceModel
	response.Diagnostics.Append(request.Plan.Get(ctx, &plan)...)
	if response.Diagnostics.HasError() {
		return
	}

	path := "/admin/clients/" + url.PathEscape(plan.ClientID.ValueString()) + "/keys"
	body := map[string]string{"key_hash": plan.KeyHash.ValueString()}
	if _, err := r.client.do(ctx, http.MethodPost, path, body, nil); err != nil {
		response.Diagnostics.AddError("client key registration failed", err.Error())
		return
	}
	response.Diagnostics.Append(response.State.Set(ctx, plan)...)
}

func (r *clientKeyResource) Read(
	ctx context.Context,
	request resource.ReadRequest,
	response *resource.ReadResponse,
) {
	var state clientKeyResourceModel
	response.Diagnostics.Append(request.State.Get(ctx, &state)...)
	if response.Diagnostics.HasError() {
		return
	}

	status, err := r.client.do(ctx, http.MethodGet, r.keyPath(state), nil, nil)
	if status == http.StatusNotFound {
		response.State.RemoveResource(ctx)
		return
	}
	if err != nil {
		response.Diagnostics.AddError("client key read failed", err.Error())
	}
}

// 모든 속성이 RequiresReplace라 도달하지 않는다.
func (r *clientKeyResource) Update(
	_ context.Context,
	_ resource.UpdateRequest,
	response *resource.UpdateResponse,
) {
	response.Diagnostics.AddError(
		"unreachable update",
		"filegate_client_key has no updatable attribute",
	)
}

func (r *clientKeyResource) Delete(
	ctx context.Context,
	request resource.DeleteRequest,
	response *resource.DeleteResponse,
) {
	var state clientKeyResourceModel
	response.Diagnostics.Append(request.State.Get(ctx, &state)...)
	if response.Diagnostics.HasError() {
		return
	}

	if _, err := r.client.do(ctx, http.MethodDelete, r.keyPath(state), nil, nil); err != nil {
		response.Diagnostics.AddError("client key delete failed", err.Error())
	}
}
