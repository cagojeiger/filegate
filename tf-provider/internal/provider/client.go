package provider

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"strings"
	"time"
)

// apiClient는 운영자 API 호출의 전부다. 인증은 Bearer 운영자 토큰.
type apiClient struct {
	baseURL string
	token   string
	http    *http.Client
}

func newAPIClient(baseURL, token string) *apiClient {
	return &apiClient{
		baseURL: strings.TrimRight(baseURL, "/"),
		token:   token,
		// 등록은 즉석 저장소 검증(head_bucket)을 동반하므로 여유를 둔다.
		http: &http.Client{Timeout: 60 * time.Second},
	}
}

// do는 JSON 요청/응답 한 번을 수행하고 HTTP 상태 코드를 돌려준다.
// 2xx가 아니면 본문의 error 메시지를 담은 에러를 함께 돌려준다.
func (c *apiClient) do(ctx context.Context, method, path string, body, out any) (int, error) {
	var reader io.Reader
	if body != nil {
		encoded, err := json.Marshal(body)
		if err != nil {
			return 0, err
		}
		reader = bytes.NewReader(encoded)
	}
	request, err := http.NewRequestWithContext(ctx, method, c.baseURL+path, reader)
	if err != nil {
		return 0, err
	}
	request.Header.Set("Authorization", "Bearer "+c.token)
	if body != nil {
		request.Header.Set("Content-Type", "application/json")
	}

	response, err := c.http.Do(request)
	if err != nil {
		return 0, err
	}
	defer response.Body.Close()

	payload, err := io.ReadAll(response.Body)
	if err != nil {
		return response.StatusCode, err
	}
	if response.StatusCode < 200 || response.StatusCode >= 300 {
		var apiError struct {
			Error string `json:"error"`
		}
		if json.Unmarshal(payload, &apiError) == nil && apiError.Error != "" {
			return response.StatusCode, fmt.Errorf("%s %s: %s", method, path, apiError.Error)
		}
		return response.StatusCode, fmt.Errorf("%s %s: HTTP %d", method, path, response.StatusCode)
	}
	if out != nil && len(payload) > 0 {
		if err := json.Unmarshal(payload, out); err != nil {
			return response.StatusCode, err
		}
	}
	return response.StatusCode, nil
}
