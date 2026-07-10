package provider

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"testing"
)

func TestAPIClientNormalizesTrailingSlash(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(response http.ResponseWriter, request *http.Request) {
		if request.URL.Path != "/admin/clients" {
			t.Fatalf("path = %q, want /admin/clients", request.URL.Path)
		}
		if request.Header.Get("Authorization") != "Bearer fgop_test" {
			t.Fatalf("Authorization header mismatch")
		}
		response.WriteHeader(http.StatusOK)
	}))
	defer server.Close()

	client := newAPIClient(server.URL+"/", "fgop_test")
	status, err := client.do(context.Background(), http.MethodGet, "/admin/clients", nil, nil)
	if err != nil {
		t.Fatalf("client.do returned error: %v", err)
	}
	if status != http.StatusOK {
		t.Fatalf("status = %d, want %d", status, http.StatusOK)
	}
}

func TestAPIClientReturnsAPIErrorMessage(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(response http.ResponseWriter, _ *http.Request) {
		response.WriteHeader(http.StatusConflict)
		if err := json.NewEncoder(response).Encode(map[string]string{"error": "already exists"}); err != nil {
			t.Fatalf("encode error response: %v", err)
		}
	}))
	defer server.Close()

	client := newAPIClient(server.URL, "fgop_test")
	status, err := client.do(context.Background(), http.MethodPost, "/admin/storages", map[string]string{"id": "x"}, nil)
	if status != http.StatusConflict {
		t.Fatalf("status = %d, want %d", status, http.StatusConflict)
	}
	if err == nil || err.Error() != "POST /admin/storages: already exists" {
		t.Fatalf("err = %v, want API error message", err)
	}
}
