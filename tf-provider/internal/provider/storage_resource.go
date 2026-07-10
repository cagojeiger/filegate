package provider

import (
	"context"
	"fmt"
	"net/http"

	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/booldefault"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/types"
)

// filegate_storage — S3 호환 저장 공간 등록 (spec 01 등록부).
// apply = 등록(즉석 접근 검증 포함), destroy = 등록 해제.
type storageResource struct {
	client *apiClient
}

func NewStorageResource() resource.Resource {
	return &storageResource{}
}

type storageResourceModel struct {
	ID             types.String `tfsdk:"id"`
	Endpoint       types.String `tfsdk:"endpoint"`
	Region         types.String `tfsdk:"region"`
	Bucket         types.String `tfsdk:"bucket"`
	ForcePathStyle types.Bool   `tfsdk:"force_path_style"`
	AccessKey      types.String `tfsdk:"access_key"`
	SecretKey      types.String `tfsdk:"secret_key"`
	CapacityBytes  types.Int64  `tfsdk:"capacity_bytes"`
}

// 운영자 API의 요청·응답 모양 (admin.rs와 일치).
type storageAPIModel struct {
	ID             string `json:"id,omitempty"`
	Endpoint       string `json:"endpoint"`
	Region         string `json:"region"`
	Bucket         string `json:"bucket"`
	ForcePathStyle bool   `json:"force_path_style"`
	AccessKey      string `json:"access_key"`
	SecretKey      string `json:"secret_key,omitempty"`
	CapacityBytes  int64  `json:"capacity_bytes"`
}

func (r *storageResource) Metadata(
	_ context.Context,
	request resource.MetadataRequest,
	response *resource.MetadataResponse,
) {
	response.TypeName = request.ProviderTypeName + "_storage"
}

func (r *storageResource) Schema(
	_ context.Context,
	_ resource.SchemaRequest,
	response *resource.SchemaResponse,
) {
	response.Schema = schema.Schema{
		Description: "S3 호환 저장 공간(storage) 등록. 등록은 그 자체가 검증이다 — " +
			"filegate가 제출된 자격증명으로 버킷 접근을 즉석 확인한다.",
		Attributes: map[string]schema.Attribute{
			"id": schema.StringAttribute{
				Required:    true,
				Description: "안정 슬러그 (생성 후 불변 — 바꾸면 재생성).",
				PlanModifiers: []planmodifier.String{
					stringplanmodifier.RequiresReplace(),
				},
			},
			"endpoint": schema.StringAttribute{
				Required:    true,
				Description: "S3 호환 endpoint URL.",
			},
			"region": schema.StringAttribute{
				Required: true,
			},
			"bucket": schema.StringAttribute{
				Required:    true,
				Description: "버킷은 미리 프로비저닝돼 있어야 한다 — filegate는 만들지 않는다.",
			},
			"force_path_style": schema.BoolAttribute{
				Optional: true,
				Computed: true,
				Default:  booldefault.StaticBool(false),
			},
			"access_key": schema.StringAttribute{
				Required: true,
			},
			"secret_key": schema.StringAttribute{
				Required:    true,
				Sensitive:   true,
				Description: "저장소 시크릿. filegate에 암호화 보관되고 다시 읽을 수 없다.",
			},
			"capacity_bytes": schema.Int64Attribute{
				Required:    true,
				Description: "이 provider에 저장할 총량 상한 (bytes).",
			},
		},
	}
}

func (r *storageResource) Configure(
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

func (r *storageResource) Create(
	ctx context.Context,
	request resource.CreateRequest,
	response *resource.CreateResponse,
) {
	var plan storageResourceModel
	response.Diagnostics.Append(request.Plan.Get(ctx, &plan)...)
	if response.Diagnostics.HasError() {
		return
	}

	body := apiModelFrom(plan)
	if _, err := r.client.do(ctx, http.MethodPost, "/admin/storages", body, nil); err != nil {
		response.Diagnostics.AddError("provider registration failed", err.Error())
		return
	}
	response.Diagnostics.Append(response.State.Set(ctx, plan)...)
}

func (r *storageResource) Read(
	ctx context.Context,
	request resource.ReadRequest,
	response *resource.ReadResponse,
) {
	var state storageResourceModel
	response.Diagnostics.Append(request.State.Get(ctx, &state)...)
	if response.Diagnostics.HasError() {
		return
	}

	var remote storageAPIModel
	status, err := r.client.do(
		ctx, http.MethodGet, "/admin/storages/"+state.ID.ValueString(), nil, &remote,
	)
	if status == http.StatusNotFound {
		// 등록부에서 사라졌다 — state에서도 지워 재생성 계획이 서게 한다.
		response.State.RemoveResource(ctx)
		return
	}
	if err != nil {
		response.Diagnostics.AddError("provider read failed", err.Error())
		return
	}

	// secret_key는 API가 돌려주지 않는다 (암호화 보관) — state 값을 유지한다.
	state.Endpoint = types.StringValue(remote.Endpoint)
	state.Region = types.StringValue(remote.Region)
	state.Bucket = types.StringValue(remote.Bucket)
	state.ForcePathStyle = types.BoolValue(remote.ForcePathStyle)
	state.AccessKey = types.StringValue(remote.AccessKey)
	state.CapacityBytes = types.Int64Value(remote.CapacityBytes)
	response.Diagnostics.Append(response.State.Set(ctx, state)...)
}

func (r *storageResource) Update(
	ctx context.Context,
	request resource.UpdateRequest,
	response *resource.UpdateResponse,
) {
	var plan storageResourceModel
	response.Diagnostics.Append(request.Plan.Get(ctx, &plan)...)
	if response.Diagnostics.HasError() {
		return
	}

	body := apiModelFrom(plan)
	body.ID = "" // id는 경로로 간다
	path := "/admin/storages/" + plan.ID.ValueString()
	if _, err := r.client.do(ctx, http.MethodPut, path, body, nil); err != nil {
		response.Diagnostics.AddError("provider update failed", err.Error())
		return
	}
	response.Diagnostics.Append(response.State.Set(ctx, plan)...)
}

func (r *storageResource) Delete(
	ctx context.Context,
	request resource.DeleteRequest,
	response *resource.DeleteResponse,
) {
	var state storageResourceModel
	response.Diagnostics.Append(request.State.Get(ctx, &state)...)
	if response.Diagnostics.HasError() {
		return
	}

	path := "/admin/storages/" + state.ID.ValueString()
	if _, err := r.client.do(ctx, http.MethodDelete, path, nil, nil); err != nil {
		// 사용 중(참조되는) provider의 삭제는 filegate가 409로 거부한다.
		response.Diagnostics.AddError("provider delete failed", err.Error())
	}
}

func apiModelFrom(model storageResourceModel) storageAPIModel {
	return storageAPIModel{
		ID:             model.ID.ValueString(),
		Endpoint:       model.Endpoint.ValueString(),
		Region:         model.Region.ValueString(),
		Bucket:         model.Bucket.ValueString(),
		ForcePathStyle: model.ForcePathStyle.ValueBool(),
		AccessKey:      model.AccessKey.ValueString(),
		SecretKey:      model.SecretKey.ValueString(),
		CapacityBytes:  model.CapacityBytes.ValueInt64(),
	}
}
