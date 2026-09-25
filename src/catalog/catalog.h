/**
 * @file catalog.h
 * @brief Built-in model catalog — the CLI's curated equivalent of the example
 *        apps' ModelCatalog (iOS ModelCatalogBootstrap.swift is the canonical
 *        reference; ids/URLs are copied verbatim from the app catalogs and the
 *        commons test tooling).
 *
 * Entries use the proto-generated enums (structured-types rule) and register
 * through the same single-call commons entry points the SDKs use:
 * rac_register_model_from_url_proto / rac_register_multi_file_model_proto.
 * Registration is idempotent per process (the registry is in-memory; apps
 * re-register their catalogs on every launch the same way).
 */

#ifndef WALLY_CATALOG_CATALOG_H
#define WALLY_CATALOG_CATALOG_H

#include <cstddef>
#include <cstdint>
#include <string>
#include <vector>

#include "model_types.pb.h"
#include "rac/core/rac_types.h"

namespace wally::catalog {

struct CatalogFile {
  const char *url;
  const char *filename;
  bool required;
  int64_t size_bytes = 0;
  const char *checksum_sha256 = nullptr;
};

struct CatalogEntry {
  const char *id;
  const char *alias; // short name accepted by `pull/run/...` (nullptr = none)
  const char *name;
  runanywhere::v1::ModelCategory category;
  runanywhere::v1::InferenceFramework framework;
  runanywhere::v1::ModelFormat format;
  const char *url; // single-file / archive primary (nullptr → multi-file)
  const CatalogFile *files; // multi-file artifacts (VLM pairs, embeddings)
  size_t file_count;
  int64_t download_size_bytes; // approximate, for display/planning
  int32_t context_length;      // 0 = unknown/not applicable
  bool supports_thinking;
  int64_t memory_required_bytes = 0; // 0 = unknown/not applicable
  const char *cua_profile = ""; // Computer-Use-Agent profile id ("" = none)
  // Shared base for the same model across backends (llama.cpp / MLX / ANE / NPU).
  // nullptr → the row stands alone under its own id. `models list` groups by this
  // and joins the backends into one row (e.g. "mlx/llama.cpp").
  const char *merge_key = nullptr;
  // Proven by the local coding-harness compatibility matrix for this exact artifact.
  bool harness_compatible = false;
};

/** All built-in entries. */
const CatalogEntry *all(size_t *count);

/** Exact id or alias lookup (nullptr when unknown). */
const CatalogEntry *find(const std::string &id_or_alias);

/** Closest-match candidates for error messages (substring match, ≤ max). */
std::vector<std::string> suggestions(const std::string &input, size_t max);

/**
 * The merge base for a registry id: the entry's merge_key when set, else the id
 * itself (unchanged when the id is not a catalog entry). Raw exact lookup with no
 * platform preference — used by `models list` to collapse a model's per-backend
 * variants into one row.
 */
std::string merge_key_for(const std::string &id);

/**
 * Register every entry with the global model registry. Logs (does not fail
 * on) individual rejections so one bad entry can't take the CLI down.
 */
rac_result_t register_all();

} // namespace wally::catalog

#endif // WALLY_CATALOG_CATALOG_H
