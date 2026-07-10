package provider

import (
	"context"
	"net/http"
	"net/url"

	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/types"
)

// filegate_binding — 클라이언트의 intent 이름을 storage에 잇는 엣지 (spec 01).
// storage_id 교체가 곧 배치 변경이다 (in-place update). 이 엣지가 남아 있는
// 동안 양끝 노드(client, storage)는 삭제가 거부된다.
type bindingResource struct {
	client *apiClient
}

func NewBindingResource() resource.Resource {
	return &bindingResource{}
}

type bindingResourceModel struct {
	ClientID  types.String `tfsdk:"client_id"`
	Intent    types.String `tfsdk:"intent"`
	StorageID types.String `tfsdk:"storage_id"`
}

func (r *bindingResource) Metadata(
	_ context.Context,
	request resource.MetadataRequest,
	response *resource.MetadataResponse,
) {
	response.TypeName = request.ProviderTypeName + "_binding"
}

func (r *bindingResource) Schema(
	_ context.Context,
	_ resource.SchemaRequest,
	response *resource.SchemaResponse,
) {
	replace := []planmodifier.String{stringplanmodifier.RequiresReplace()}
	response.Schema = schema.Schema{
		Description: "클라이언트의 intent 이름을 storage에 잇는 연결. " +
			"storage_id 변경은 배치 변경이다 — 새 파일만 새 곳으로 간다 (v0).",
		Attributes: map[string]schema.Attribute{
			"client_id": schema.StringAttribute{
				Required:      true,
				PlanModifiers: replace,
			},
			"intent": schema.StringAttribute{
				Required:      true,
				Description:   "서비스가 쓰는 파일 용도 이름. 서비스 계약이라 불변이다.",
				PlanModifiers: replace,
			},
			"storage_id": schema.StringAttribute{
				Required:    true,
				Description: "파일이 저장될 storage. 교체 가능 (in-place).",
			},
		},
	}
}

func (r *bindingResource) Configure(
	_ context.Context,
	request resource.ConfigureRequest,
	response *resource.ConfigureResponse,
) {
	r.client = configureAPIClient(request, response)
}

func (r *bindingResource) bindingPath(model bindingResourceModel) string {
	return "/admin/clients/" + url.PathEscape(model.ClientID.ValueString()) +
		"/bindings/" + url.PathEscape(model.Intent.ValueString())
}

// Create=POST(중복이면 409 — 기존 binding을 조용히 덮지 않는다),
// Update=PUT(갱신 전용, 없으면 404). TF 라이프사이클과 1:1이다.
func (r *bindingResource) send(
	ctx context.Context,
	method string,
	model bindingResourceModel,
) error {
	body := map[string]string{"storage_id": model.StorageID.ValueString()}
	_, err := r.client.do(ctx, method, r.bindingPath(model), body, nil)
	return err
}

func (r *bindingResource) Create(
	ctx context.Context,
	request resource.CreateRequest,
	response *resource.CreateResponse,
) {
	var plan bindingResourceModel
	response.Diagnostics.Append(request.Plan.Get(ctx, &plan)...)
	if response.Diagnostics.HasError() {
		return
	}
	if err := r.send(ctx, http.MethodPost, plan); err != nil {
		response.Diagnostics.AddError("binding failed", err.Error())
		return
	}
	response.Diagnostics.Append(response.State.Set(ctx, plan)...)
}

func (r *bindingResource) Read(
	ctx context.Context,
	request resource.ReadRequest,
	response *resource.ReadResponse,
) {
	var state bindingResourceModel
	response.Diagnostics.Append(request.State.Get(ctx, &state)...)
	if response.Diagnostics.HasError() {
		return
	}

	var remote struct {
		StorageID string `json:"storage_id"`
	}
	status, err := r.client.do(ctx, http.MethodGet, r.bindingPath(state), nil, &remote)
	if status == http.StatusNotFound {
		response.State.RemoveResource(ctx)
		return
	}
	if err != nil {
		response.Diagnostics.AddError("binding read failed", err.Error())
		return
	}
	state.StorageID = types.StringValue(remote.StorageID)
	response.Diagnostics.Append(response.State.Set(ctx, state)...)
}

func (r *bindingResource) Update(
	ctx context.Context,
	request resource.UpdateRequest,
	response *resource.UpdateResponse,
) {
	var plan bindingResourceModel
	response.Diagnostics.Append(request.Plan.Get(ctx, &plan)...)
	if response.Diagnostics.HasError() {
		return
	}
	if err := r.send(ctx, http.MethodPut, plan); err != nil {
		response.Diagnostics.AddError("binding update failed", err.Error())
		return
	}
	response.Diagnostics.Append(response.State.Set(ctx, plan)...)
}

func (r *bindingResource) Delete(
	ctx context.Context,
	request resource.DeleteRequest,
	response *resource.DeleteResponse,
) {
	var state bindingResourceModel
	response.Diagnostics.Append(request.State.Get(ctx, &state)...)
	if response.Diagnostics.HasError() {
		return
	}

	if _, err := r.client.do(ctx, http.MethodDelete, r.bindingPath(state), nil, nil); err != nil {
		response.Diagnostics.AddError("binding delete failed", err.Error())
	}
}
