package provider

import (
	"fmt"

	"github.com/hashicorp/terraform-plugin-framework/resource"
)

func configureAPIClient(
	request resource.ConfigureRequest,
	response *resource.ConfigureResponse,
) *apiClient {
	if request.ProviderData == nil {
		return nil
	}
	client, ok := request.ProviderData.(*apiClient)
	if !ok {
		response.Diagnostics.AddError(
			"unexpected provider data",
			fmt.Sprintf("expected *apiClient, got %T", request.ProviderData),
		)
		return nil
	}
	return client
}
