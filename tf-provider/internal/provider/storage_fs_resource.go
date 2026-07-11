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

// filegate_storage_fs — 로컬/NFS 파일시스템 storage 등록 (ADR 001).
// root_path가 접근 계약의 전부인, 시크릿 없는 storage다. presigned 개념이
// 없어 항상 중계이므로 filegate에 FILEGATE_PUBLIC_URL이 서 있어야 한다.
type storageFsResource struct {
	client *apiClient
}

func NewStorageFsResource() resource.Resource {
	return &storageFsResource{}
}

type storageFsResourceModel struct {
	ID            types.String `tfsdk:"id"`
	RootPath      types.String `tfsdk:"root_path"`
	CapacityBytes types.Int64  `tfsdk:"capacity_bytes"`
}

func (r *storageFsResource) Metadata(
	_ context.Context,
	request resource.MetadataRequest,
	response *resource.MetadataResponse,
) {
	response.TypeName = request.ProviderTypeName + "_storage_fs"
}

func (r *storageFsResource) Schema(
	_ context.Context,
	_ resource.SchemaRequest,
	response *resource.SchemaResponse,
) {
	response.Schema = schema.Schema{
		Description: "파일시스템(로컬/NFS) storage 등록. 등록은 그 자체가 검증이다 — " +
			"filegate가 경로 존재와 쓰기 가능을 즉석 확인한다. 항상 중계 모드다.",
		Attributes: map[string]schema.Attribute{
			"id": schema.StringAttribute{
				Required:    true,
				Description: "안정 슬러그 (생성 후 불변 — 바꾸면 재생성).",
				PlanModifiers: []planmodifier.String{
					stringplanmodifier.RequiresReplace(),
				},
			},
			"root_path": schema.StringAttribute{
				Required: true,
				Description: "filegate 프로세스가 접근하는 디렉토리. 멀티 파드면 " +
					"모든 파드가 같은 마운트를 공유해야 한다.",
			},
			"capacity_bytes": schema.Int64Attribute{
				Required:    true,
				Description: "이 storage에 저장할 총량 상한 (bytes).",
			},
		},
	}
}

func (r *storageFsResource) Configure(
	_ context.Context,
	request resource.ConfigureRequest,
	response *resource.ConfigureResponse,
) {
	r.client = configureAPIClient(request, response)
}

func fsAPIModelFrom(model storageFsResourceModel) storageAPIModel {
	return storageAPIModel{
		ID:            model.ID.ValueString(),
		Kind:          "fs",
		RootPath:      model.RootPath.ValueString(),
		CapacityBytes: model.CapacityBytes.ValueInt64(),
	}
}

func (r *storageFsResource) Create(
	ctx context.Context,
	request resource.CreateRequest,
	response *resource.CreateResponse,
) {
	var plan storageFsResourceModel
	response.Diagnostics.Append(request.Plan.Get(ctx, &plan)...)
	if response.Diagnostics.HasError() {
		return
	}
	body := fsAPIModelFrom(plan)
	if _, err := r.client.do(ctx, http.MethodPost, "/admin/storages", body, nil); err != nil {
		response.Diagnostics.AddError("storage registration failed", err.Error())
		return
	}
	response.Diagnostics.Append(response.State.Set(ctx, plan)...)
}

func (r *storageFsResource) Read(
	ctx context.Context,
	request resource.ReadRequest,
	response *resource.ReadResponse,
) {
	var state storageFsResourceModel
	response.Diagnostics.Append(request.State.Get(ctx, &state)...)
	if response.Diagnostics.HasError() {
		return
	}
	var remote storageAPIModel
	status, err := r.client.do(
		ctx,
		http.MethodGet,
		"/admin/storages/"+url.PathEscape(state.ID.ValueString()),
		nil,
		&remote,
	)
	if status == http.StatusNotFound {
		response.State.RemoveResource(ctx)
		return
	}
	if err != nil {
		response.Diagnostics.AddError("storage read failed", err.Error())
		return
	}
	state.RootPath = types.StringValue(remote.RootPath)
	state.CapacityBytes = types.Int64Value(remote.CapacityBytes)
	response.Diagnostics.Append(response.State.Set(ctx, state)...)
}

func (r *storageFsResource) Update(
	ctx context.Context,
	request resource.UpdateRequest,
	response *resource.UpdateResponse,
) {
	var plan storageFsResourceModel
	response.Diagnostics.Append(request.Plan.Get(ctx, &plan)...)
	if response.Diagnostics.HasError() {
		return
	}
	body := fsAPIModelFrom(plan)
	body.ID = "" // id는 경로로 간다
	path := "/admin/storages/" + url.PathEscape(plan.ID.ValueString())
	if _, err := r.client.do(ctx, http.MethodPut, path, body, nil); err != nil {
		response.Diagnostics.AddError("storage update failed", err.Error())
		return
	}
	response.Diagnostics.Append(response.State.Set(ctx, plan)...)
}

func (r *storageFsResource) Delete(
	ctx context.Context,
	request resource.DeleteRequest,
	response *resource.DeleteResponse,
) {
	var state storageFsResourceModel
	response.Diagnostics.Append(request.State.Get(ctx, &state)...)
	if response.Diagnostics.HasError() {
		return
	}
	path := "/admin/storages/" + url.PathEscape(state.ID.ValueString())
	if _, err := r.client.do(ctx, http.MethodDelete, path, nil, nil); err != nil {
		response.Diagnostics.AddError("storage delete failed", err.Error())
	}
}
