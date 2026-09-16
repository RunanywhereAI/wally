package runanywhere

import (
	"context"
	"errors"
)

// DownloadProgress is a snapshot of an in-flight model download. Total is 0
// when the size is not yet known.
type DownloadProgress struct {
	Downloaded int64
	Total      int64
}

// Downloader fetches a model by id into the on-device model store.
// onProgress may be called from any goroutine and is never nil to a
// Downloader implementation that is willing to report progress; callers
// that do not care about progress pass a func that discards it.
type Downloader interface {
	Download(ctx context.Context, id string, onProgress func(DownloadProgress)) error
}

var downloader Downloader

func SetDownloader(d Downloader) { downloader = d }

// DownloadEnabled reports whether a build has linked a downloader, so a
// caller can avoid offering a download it cannot actually run.
func DownloadEnabled() bool { return downloader != nil }

// ErrDownloadNotWired is returned by DownloadModel until the ondevice build
// installs a real downloader; the cloud branch never needs one.
var ErrDownloadNotWired = errors.New("model download is not enabled in this build yet")

// DownloadModel fetches model id into the on-device store, reporting
// progress through onProgress (which may be nil).
func DownloadModel(ctx context.Context, id string, onProgress func(DownloadProgress)) error {
	if downloader == nil {
		return ErrDownloadNotWired
	}
	return downloader.Download(ctx, id, onProgress)
}

// CatalogLister is an optional Downloader capability that reports the model ids
// available to download, so `wally models list` can show what pull accepts.
type CatalogLister interface {
	DownloadCatalog() []string
}

// DownloadableModels returns the ids pull accepts, or nil when the build has no
// downloader or it lists no catalog.
func DownloadableModels() []string {
	if c, ok := downloader.(CatalogLister); ok {
		return c.DownloadCatalog()
	}
	return nil
}
