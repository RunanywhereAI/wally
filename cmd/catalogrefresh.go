package cmd

import (
	"context"
	"time"

	"github.com/RunanywhereAI/wally/catalog"
)

const catalogRefreshInterval = 30 * time.Minute

// refreshCatalog fetches the cloud catalog for the signed-in session and writes
// it to the local cache. Signed out or unreachable is not surfaced; the cache
// keeps its last good contents.
//
// Prices and model limits are two different endpoints, so they are fetched
// and merged by ID rather than assumed to arrive together: a Models failure
// still saves prices, with ContextWindow/MaxOutputTokens left at zero
// ("unknown") rather than blocking the whole refresh.
func refreshCatalog(ctx context.Context, sess *session) error {
	token, err := sess.Token()
	if err != nil {
		return err
	}
	prices, err := sess.client.Catalog(ctx, token)
	if err != nil {
		return err
	}
	models := make([]catalog.Model, len(prices))
	for i, p := range prices {
		models[i] = catalog.Model{ID: p.ID, InputPerMTok: p.InputPerMTok, OutputPerMTok: p.OutputPerMTok}
	}

	if limits, err := sess.client.Models(ctx, token); err == nil {
		byID := make(map[string]int, len(models))
		for i, m := range models {
			byID[m.ID] = i
		}
		for _, l := range limits {
			if i, ok := byID[l.ID]; ok {
				models[i].ContextWindow = l.ContextWindow
				models[i].MaxOutputTokens = l.MaxOutputTokens
			}
		}
	}

	return catalog.Save(models)
}

// refreshCatalogLoop keeps the local catalog cache warm while the daemon runs.
// Best-effort: a failed refresh leaves the last cache in place.
func refreshCatalogLoop(ctx context.Context, sess *session) {
	_ = refreshCatalog(ctx, sess)
	ticker := time.NewTicker(catalogRefreshInterval)
	defer ticker.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
			_ = refreshCatalog(ctx, sess)
		}
	}
}

// warmCatalog does a one-time bounded fetch to populate an empty cache, so the
// very first model check works without waiting on the daemon's background loop.
// Bounded and best-effort: a slow or unreachable console returns nil, never a
// hang.
func warmCatalog() []catalog.Model {
	sess, err := newSession()
	if err != nil || !sess.signedIn() {
		return nil
	}
	ctx, cancel := context.WithTimeout(context.Background(), 3*time.Second)
	defer cancel()
	if err := refreshCatalog(ctx, sess); err != nil {
		return nil
	}
	models, _ := catalog.Load()
	return models
}
