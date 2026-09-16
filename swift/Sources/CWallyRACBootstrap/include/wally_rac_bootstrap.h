#ifndef WALLY_RAC_BOOTSTRAP_H
#define WALLY_RAC_BOOTSTRAP_H

// Thin forwarding shim, same pattern as wally-legacy/swift/Sources/CWallyApp:
// the real struct/function declarations live in the packaged C++ desktop
// kit's headers (see Package.swift's kitInclude header search path), reused
// here header-only. This target links no C++ object code of its own --
// rac_init/rac_set_platform_adapter/rac_logger_*/rac_model_paths_set_base_dir
// resolve at link time against RunAnywhereMLX's RACommonsBinary.xcframework.
#include "rac/core/rac_core.h"
#include "rac/core/rac_logger.h"
#include "rac/core/rac_platform_adapter.h"
#include "rac/infrastructure/model_management/rac_model_paths.h"
#include "rac/infrastructure/model_management/rac_model_registry.h"
#include "rac/core/rac_model_lifecycle.h"
#include "rac/features/llm/rac_llm_service.h"
#include "rac/foundation/rac_proto_buffer.h"

#endif /* WALLY_RAC_BOOTSTRAP_H */
