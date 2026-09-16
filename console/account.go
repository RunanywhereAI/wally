package console

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"net/url"
)

// Identity is what GET /v1/me answers with for a signed-in session.
type Identity struct {
	Email             string
	Plan              string
	TokensThisMonth   int64
	MonthlyTokenLimit int64
}

type identityResponse struct {
	Email             string `json:"email"`
	Plan              string `json:"plan"`
	TokensThisMonth   int64  `json:"tokens_this_month"`
	MonthlyTokenLimit int64  `json:"monthly_token_limit"`
}

// WhoAmI confirms a session and returns the identity behind it.
func (c *Client) WhoAmI(ctx context.Context, accessToken string) (*Identity, error) {
	if !sessionTokenSafe(accessToken) {
		return nil, errors.New("no access token is available")
	}
	resp, data, err := c.request(ctx, http.MethodGet, "/v1/me", accessToken, nil)
	if err != nil {
		return nil, err
	}
	if resp.StatusCode != http.StatusOK {
		return nil, c.apiError("identity request", resp)
	}

	var parsed identityResponse
	if err := json.Unmarshal(data, &parsed); err != nil {
		return nil, fmt.Errorf("console returned a response that did not match the contract: %w", err)
	}
	if !displaySafe(parsed.Email, 320) {
		return nil, errors.New("console returned an invalid account identity")
	}
	if parsed.TokensThisMonth < 0 || parsed.MonthlyTokenLimit < 0 {
		return nil, errors.New("console returned invalid account usage")
	}
	return &Identity{
		Email:             parsed.Email,
		Plan:              parsed.Plan,
		TokensThisMonth:   parsed.TokensThisMonth,
		MonthlyTokenLimit: parsed.MonthlyTokenLimit,
	}, nil
}

// UsageQuery narrows GET /v1/cli/usage. Days and Limit are clamped to the
// console's own bounds; a zero value clamps to the minimum rather than
// silently defaulting, so callers that want the console's default pass its
// actual value explicitly.
type UsageQuery struct {
	Days  int
	Model string
	Limit int
}

// Credit is the account's prepaid balance. Money is integer micro-dollars
// everywhere: one dollar is 1,000,000, and a single request routinely costs a
// few hundred.
type Credit struct {
	BalanceMicros int64
	GrantedMicros int64
	SpentMicros   int64
}

type UsageTotals struct {
	Requests         int64
	PromptTokens     int64
	CompletionTokens int64
	CachedTokens     int64
	CostMicros       int64
}

// UsageWindow is spend over a window ending now, totalled by the console.
// Empty against a console that predates windowed totals; callers render what
// is missing as missing rather than substituting a wider window's numbers.
type UsageWindow struct {
	Window  string // "1h" or "24h", as the console labels it
	Seconds int64
	Totals  UsageTotals
}

type UsageDay struct {
	Date             string
	Requests         int64
	PromptTokens     int64
	CompletionTokens int64
	CostMicros       int64
}

type UsageModel struct {
	Model            string
	Requests         int64
	PromptTokens     int64
	CompletionTokens int64
	CachedTokens     int64
	CostMicros       int64
}

type UsageEvent struct {
	RequestID        string
	Model            string
	Harness          string
	StartedAt        string
	ErrorCode        string
	PromptTokens     int64
	CompletionTokens int64
	CachedTokens     int64
	CostMicros       int64
	TTFTMillis       int64
	StatusCode       int
}

// Usage is one read for the whole usage report: a terminal draws it in a
// single pass, and separate round trips for credit, totals, and history would
// only give three chances to disagree about when they were taken.
type Usage struct {
	Credit   Credit
	Totals   UsageTotals
	Windows  []UsageWindow
	Timeline []UsageDay
	Models   []UsageModel
	Events   []UsageEvent
}

type usageTotalsWire struct {
	Requests         int64 `json:"requests"`
	PromptTokens     int64 `json:"prompt_tokens"`
	CompletionTokens int64 `json:"completion_tokens"`
	CachedTokens     int64 `json:"cached_tokens"`
	CostMicros       int64 `json:"cost_micros"`
}

func (w usageTotalsWire) toDomain() UsageTotals {
	return UsageTotals{
		Requests:         w.Requests,
		PromptTokens:     w.PromptTokens,
		CompletionTokens: w.CompletionTokens,
		CachedTokens:     w.CachedTokens,
		CostMicros:       w.CostMicros,
	}
}

type usageResponse struct {
	Credit struct {
		BalanceMicros int64 `json:"balance_micros"`
		GrantedMicros int64 `json:"granted_micros"`
		SpentMicros   int64 `json:"spent_micros"`
	} `json:"credit"`
	Totals  usageTotalsWire `json:"totals"`
	Windows []struct {
		Window  string          `json:"window"`
		Seconds int64           `json:"seconds"`
		Totals  usageTotalsWire `json:"totals"`
	} `json:"windows"`
	Timeline []struct {
		Date             string `json:"date"`
		Requests         int64  `json:"requests"`
		PromptTokens     int64  `json:"prompt_tokens"`
		CompletionTokens int64  `json:"completion_tokens"`
		CostMicros       int64  `json:"cost_micros"`
	} `json:"timeline"`
	Models []struct {
		Model            string `json:"model"`
		Requests         int64  `json:"requests"`
		PromptTokens     int64  `json:"prompt_tokens"`
		CompletionTokens int64  `json:"completion_tokens"`
		CachedTokens     int64  `json:"cached_tokens"`
		CostMicros       int64  `json:"cost_micros"`
	} `json:"models"`
	Recent []struct {
		RequestID        string `json:"request_id"`
		Model            string `json:"model"`
		Harness          string `json:"harness"`
		TsStart          string `json:"ts_start"`
		ErrorCode        string `json:"error_code"`
		PromptTokens     int64  `json:"prompt_tokens"`
		CompletionTokens int64  `json:"completion_tokens"`
		CachedTokens     int64  `json:"cached_tokens"`
		CostMicros       int64  `json:"cost_micros"`
		TTFTMillis       int64  `json:"ttft_ms"`
		StatusCode       int    `json:"status_code"`
	} `json:"recent"`
}

// usageWindowLabelValid mirrors the console's closed enum for a usage
// window's label. An unrecognized label fails rather than rendering a spend
// number under a name nobody chose.
func usageWindowLabelValid(label string) bool {
	return label == "1h" || label == "24h"
}

// Usage fetches the account's spend report.
func (c *Client) Usage(ctx context.Context, accessToken string, query UsageQuery) (*Usage, error) {
	if !sessionTokenSafe(accessToken) {
		return nil, errors.New("no access token is available")
	}
	days := clampInt(query.Days, 1, 365)
	limit := clampInt(query.Limit, 1, 200)
	path := fmt.Sprintf("/v1/cli/usage?days=%d&limit=%d", days, limit)
	if query.Model != "" {
		path += "&model=" + url.QueryEscape(query.Model)
	}

	resp, data, err := c.request(ctx, http.MethodGet, path, accessToken, nil)
	if err != nil {
		return nil, err
	}
	if resp.StatusCode != http.StatusOK {
		return nil, c.apiError("usage request", resp)
	}

	var parsed usageResponse
	if err := json.Unmarshal(data, &parsed); err != nil {
		return nil, fmt.Errorf("console returned a response that did not match the contract: %w", err)
	}

	usage := &Usage{
		Credit: Credit{
			BalanceMicros: parsed.Credit.BalanceMicros,
			GrantedMicros: parsed.Credit.GrantedMicros,
			SpentMicros:   parsed.Credit.SpentMicros,
		},
		Totals: parsed.Totals.toDomain(),
	}
	for _, w := range parsed.Windows {
		if !usageWindowLabelValid(w.Window) {
			return nil, fmt.Errorf("console returned an unknown usage window %q", w.Window)
		}
		usage.Windows = append(usage.Windows, UsageWindow{Window: w.Window, Seconds: w.Seconds, Totals: w.Totals.toDomain()})
	}
	for _, d := range parsed.Timeline {
		usage.Timeline = append(usage.Timeline, UsageDay{
			Date: displayOr(d.Date, 32), Requests: d.Requests,
			PromptTokens: d.PromptTokens, CompletionTokens: d.CompletionTokens, CostMicros: d.CostMicros,
		})
	}
	for _, m := range parsed.Models {
		usage.Models = append(usage.Models, UsageModel{
			Model: displayOr(m.Model, 128), Requests: m.Requests,
			PromptTokens: m.PromptTokens, CompletionTokens: m.CompletionTokens,
			CachedTokens: m.CachedTokens, CostMicros: m.CostMicros,
		})
	}
	for _, e := range parsed.Recent {
		usage.Events = append(usage.Events, UsageEvent{
			RequestID: displayOr(e.RequestID, 128), Model: displayOr(e.Model, 128),
			Harness: displayOr(e.Harness, 64), StartedAt: displayOr(e.TsStart, 64),
			ErrorCode: displayOr(e.ErrorCode, 64), PromptTokens: e.PromptTokens,
			CompletionTokens: e.CompletionTokens, CachedTokens: e.CachedTokens,
			CostMicros: e.CostMicros, TTFTMillis: e.TTFTMillis, StatusCode: e.StatusCode,
		})
	}
	return usage, nil
}

// ModelInfo is one served model as /v1/models advertises it. ContextWindow is
// the input-token ceiling a coding agent reads to decide when to compact;
// zero means the deployment declared none.
type ModelInfo struct {
	ID              string
	ContextWindow   int64
	MaxOutputTokens int64
}

type modelListResponse struct {
	Data []struct {
		ID              string `json:"id"`
		MaxInputTokens  int64  `json:"max_input_tokens"`
		MaxOutputTokens int64  `json:"max_output_tokens"`
	} `json:"data"`
}

// Models fetches the served model catalog, used to feed a harness the real
// context window so its auto-compaction fires at the right point.
func (c *Client) Models(ctx context.Context, accessToken string) ([]ModelInfo, error) {
	if !sessionTokenSafe(accessToken) {
		return nil, errors.New("no access token is available")
	}
	resp, data, err := c.request(ctx, http.MethodGet, "/v1/models", accessToken, nil)
	if err != nil {
		return nil, err
	}
	if resp.StatusCode != http.StatusOK {
		return nil, c.apiError("models request", resp)
	}

	var parsed modelListResponse
	if err := json.Unmarshal(data, &parsed); err != nil {
		return nil, fmt.Errorf("console returned a response that did not match the contract: %w", err)
	}
	models := make([]ModelInfo, 0, len(parsed.Data))
	for _, m := range parsed.Data {
		models = append(models, ModelInfo{ID: m.ID, ContextWindow: m.MaxInputTokens, MaxOutputTokens: m.MaxOutputTokens})
	}
	return models, nil
}

// CatalogPrice is one model's price, in micro-dollars per million tokens
// (1 USD = 1,000,000 micros), straight from the catalog the credit gate
// charges against.
type CatalogPrice struct {
	ID            string
	InputPerMTok  int64
	OutputPerMTok int64
}

type catalogResponse struct {
	Models []struct {
		ID            string `json:"id"`
		InputPerMTok  int64  `json:"input_per_mtok"`
		OutputPerMTok int64  `json:"output_per_mtok"`
	} `json:"models"`
}

// Catalog fetches per-model pricing, so a harness can show real spend instead
// of $0.00.
func (c *Client) Catalog(ctx context.Context, accessToken string) ([]CatalogPrice, error) {
	if !sessionTokenSafe(accessToken) {
		return nil, errors.New("no access token is available")
	}
	resp, data, err := c.request(ctx, http.MethodGet, "/v1/models/catalog", accessToken, nil)
	if err != nil {
		return nil, err
	}
	if resp.StatusCode != http.StatusOK {
		return nil, c.apiError("model catalog request", resp)
	}

	var parsed catalogResponse
	if err := json.Unmarshal(data, &parsed); err != nil {
		return nil, fmt.Errorf("console returned a response that did not match the contract: %w", err)
	}
	prices := make([]CatalogPrice, 0, len(parsed.Models))
	for _, m := range parsed.Models {
		prices = append(prices, CatalogPrice{ID: m.ID, InputPerMTok: m.InputPerMTok, OutputPerMTok: m.OutputPerMTok})
	}
	return prices, nil
}

func clampInt(v, min, max int) int {
	if v < min {
		return min
	}
	if v > max {
		return max
	}
	return v
}

// displayOr returns value when it is safe to render, or an empty string
// otherwise. A console string reaches the terminal or the credential store
// directly; anything outside plain printable ASCII is dropped rather than
// rendered.
func displayOr(value string, max int) string {
	if displaySafe(value, max) {
		return value
	}
	return ""
}
