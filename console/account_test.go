package console

import (
	"context"
	"errors"
	"net/http"
	"testing"
)

func TestWhoAmI_Success(t *testing.T) {
	client := newTestClient(t, func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("Authorization") != "Bearer at-123" {
			t.Fatalf("missing bearer header: %q", r.Header.Get("Authorization"))
		}
		w.Header().Set("Content-Type", "application/json")
		w.Write([]byte(`{"email":"person@example.com","plan":"beta","tokens_this_month":100,"monthly_token_limit":1000}`))
	})

	identity, err := client.WhoAmI(context.Background(), "at-123")
	if err != nil {
		t.Fatalf("WhoAmI() error = %v", err)
	}
	if identity.Email != "person@example.com" || identity.Plan != "beta" {
		t.Fatalf("unexpected identity: %+v", identity)
	}
}

func TestWhoAmI_MissingFieldsDefault(t *testing.T) {
	client := newTestClient(t, func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		w.Write([]byte(`{"email":"person@example.com"}`))
	})

	identity, err := client.WhoAmI(context.Background(), "at-123")
	if err != nil {
		t.Fatalf("WhoAmI() error = %v", err)
	}
	if identity.Plan != "" || identity.TokensThisMonth != 0 || identity.MonthlyTokenLimit != 0 {
		t.Fatalf("expected omitted fields to default to zero, got %+v", identity)
	}
}

func TestWhoAmI_Unauthorized(t *testing.T) {
	client := newTestClient(t, func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusUnauthorized)
	})

	_, err := client.WhoAmI(context.Background(), "at-123")
	if !errors.Is(err, ErrUnauthorized) {
		t.Fatalf("WhoAmI() error = %v, want it to wrap ErrUnauthorized", err)
	}
}

func TestWhoAmI_NegativeUsageFails(t *testing.T) {
	client := newTestClient(t, func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		w.Write([]byte(`{"email":"person@example.com","tokens_this_month":-1}`))
	})

	if _, err := client.WhoAmI(context.Background(), "at-123"); err == nil {
		t.Fatal("expected negative usage from the console to fail rather than render")
	}
}

func TestUsage_Success(t *testing.T) {
	client := newTestClient(t, func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Query().Get("days") != "7" || r.URL.Query().Get("limit") != "20" {
			t.Fatalf("unexpected query: %s", r.URL.RawQuery)
		}
		w.Header().Set("Content-Type", "application/json")
		w.Write([]byte(`{
			"credit": {"balance_micros": 500000, "granted_micros": 1000000, "spent_micros": 500000},
			"totals": {"requests": 10, "prompt_tokens": 100, "completion_tokens": 50, "cached_tokens": 5, "cost_micros": 1200},
			"windows": [{"window": "1h", "seconds": 3600, "totals": {"requests": 2, "prompt_tokens": 20, "completion_tokens": 10, "cached_tokens": 0, "cost_micros": 200}}],
			"timeline": [{"date": "2026-09-15", "requests": 10, "prompt_tokens": 100, "completion_tokens": 50, "cost_micros": 1200}],
			"models": [{"model": "gpt-oss-20b", "requests": 10, "prompt_tokens": 100, "completion_tokens": 50, "cached_tokens": 5, "cost_micros": 1200}],
			"recent": [{"request_id": "req-1", "model": "gpt-oss-20b", "harness": "claude_code", "ts_start": "2026-09-15T00:00:00Z", "prompt_tokens": 10, "completion_tokens": 5, "cached_tokens": 0, "cost_micros": 120, "status_code": 200}]
		}`))
	})

	usage, err := client.Usage(context.Background(), "at-123", UsageQuery{Days: 7, Limit: 20})
	if err != nil {
		t.Fatalf("Usage() error = %v", err)
	}
	if usage.Credit.BalanceMicros != 500000 {
		t.Fatalf("unexpected credit: %+v", usage.Credit)
	}
	if len(usage.Windows) != 1 || usage.Windows[0].Window != "1h" {
		t.Fatalf("unexpected windows: %+v", usage.Windows)
	}
	if len(usage.Events) != 1 || usage.Events[0].RequestID != "req-1" {
		t.Fatalf("unexpected events: %+v", usage.Events)
	}
}

func TestUsage_UnknownWindowLabelFails(t *testing.T) {
	client := newTestClient(t, func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		w.Write([]byte(`{"windows": [{"window": "7d", "seconds": 604800, "totals": {}}]}`))
	})

	if _, err := client.Usage(context.Background(), "at-123", UsageQuery{Days: 7, Limit: 20}); err == nil {
		t.Fatal("expected an unrecognized usage window label to fail")
	}
}

func TestModels_Success(t *testing.T) {
	client := newTestClient(t, func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		w.Write([]byte(`{"data":[{"id":"gpt-oss-20b","max_input_tokens":128000},{"id":"local-only"}]}`))
	})

	models, err := client.Models(context.Background(), "at-123")
	if err != nil {
		t.Fatalf("Models() error = %v", err)
	}
	if len(models) != 2 {
		t.Fatalf("len(models) = %d, want 2", len(models))
	}
	if models[0].ContextWindow != 128000 {
		t.Fatalf("unexpected context window: %+v", models[0])
	}
	if models[1].ContextWindow != 0 || models[1].MaxOutputTokens != 0 {
		t.Fatalf("expected omitted token limits to default to zero, got %+v", models[1])
	}
}

func TestCatalog_Success(t *testing.T) {
	client := newTestClient(t, func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		w.Write([]byte(`{"models":[{"id":"gpt-oss-20b","input_per_mtok":100,"output_per_mtok":400}]}`))
	})

	prices, err := client.Catalog(context.Background(), "at-123")
	if err != nil {
		t.Fatalf("Catalog() error = %v", err)
	}
	if len(prices) != 1 || prices[0].InputPerMTok != 100 || prices[0].OutputPerMTok != 400 {
		t.Fatalf("unexpected prices: %+v", prices)
	}
}
