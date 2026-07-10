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

func (r *bindingResource) bindingPath(model bindingResourceModel) string {
	return "/admin/clients/" + model.ClientID.ValueString() +
		"/bindings/" + model.Intent.ValueString()
}

// PUT은 upsert라 Create와 Update가 같은 호출이다.
func (r *bindingResource) put(ctx context.Context, model bindingResourceModel) error {
	body := map[string]string{"storage_id": model.StorageID.ValueString()}
	_, err := r.client.do(ctx, http.MethodPut, r.bindingPath(model), body, nil)
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
	if err := r.put(ctx, plan); err != nil {
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
	if err := r.put(ctx, plan); err != nil {
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
