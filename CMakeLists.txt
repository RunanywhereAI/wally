cmake_minimum_required(VERSION 3.24)

# The product version is read from versions.toml, the single source for every
# version and pin in this repo -- never hardcoded here. Parsed before project()
# with a plain regex, since versions.toml is flat `key = "value"` by design.
file(STRINGS "${CMAKE_CURRENT_SOURCE_DIR}/versions.toml" _wally_version_line
     REGEX "^[ \t]*version[ \t]*=")
if(NOT _wally_version_line)
    message(FATAL_ERROR "versions.toml is missing the product version")
endif()
list(GET _wally_version_line 0 _wally_version_line)
string(REGEX REPLACE "^[^\"]*\"([^\"]*)\".*$" "\\1" _wally_version "${_wally_version_line}")

project(wally VERSION "${_wally_version}" LANGUAGES C CXX)

# The SDK kit pin (cmake/sdk-pin.cmake, also from versions.toml) moves on its
# own cadence. scripts/build/package-wally.sh and the release workflow stamp the
# product version into Formula/wally.rb; scripts/ci/check-versions.py fails a build
# where the formula or the Swift package pin has drifted from versions.toml.

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)

if(NOT CMAKE_BUILD_TYPE AND NOT CMAKE_CONFIGURATION_TYPES)
    set(CMAKE_BUILD_TYPE Release)
endif()

list(APPEND CMAKE_MODULE_PATH "${CMAKE_CURRENT_SOURCE_DIR}/cmake")
include(RunAnywhereSDK)

# In-tree wally sources (copied from runanywhere-sdks/wally). Command files are
# thin: parse → bootstrap() → one rac_* call → render.
set(WALLY_SOURCES
    src/app.cpp
    src/cli_formatter.cpp
    src/bootstrap.cpp
    src/catalog/catalog.cpp
    src/catalog/model_ref.cpp
    src/commands/cmd_version.cpp
    src/commands/cmd_info.cpp
    src/commands/cmd_about.cpp
    src/commands/cmd_auth.cpp
    src/commands/cmd_backends.cpp
    src/commands/cmd_list.cpp
    src/commands/cmd_lora.cpp
    src/commands/cmd_models.cpp
    src/commands/cmd_pull.cpp
    src/commands/cmd_rerank.cpp
    src/commands/cmd_rm.cpp
    src/commands/cmd_run.cpp
    src/commands/cmd_tool.cpp
    src/commands/cmd_image.cpp
    src/commands/cmd_segment.cpp
    src/commands/cmd_serve.cpp
    src/commands/cmd_show.cpp
    src/commands/cmd_stt.cpp
    src/commands/cmd_diarize.cpp
    src/commands/cmd_embed.cpp
    src/commands/cmd_telemetry.cpp
    src/commands/cmd_tts.cpp
    src/commands/cmd_vad.cpp
    src/commands/cmd_voice.cpp
    src/commands/cmd_rag.cpp
    src/commands/cmd_bench.cpp
    src/commands/cmd_editors.cpp
    src/commands/cmd_harness.cpp
    src/commands/cmd_default_models.cpp
    src/commands/cmd_account.cpp
    src/commands/cmd_usage.cpp
    src/account/console.cpp
    src/account/credentials.cpp
    src/anthropic/messages.cpp
    src/anthropic/translate.cpp
    src/desktop/claude_profile.cpp
    src/harness/harness.cpp
    src/harness/opencode.cpp
    src/harness/codex.cpp
    src/harness/local_models.cpp
    src/ide/jetbrains_profile.cpp
    src/ide/openai_proxy.cpp
    src/commands/engine_options.cpp
    src/commands/model_setup.cpp
    src/config/cli_paths.cpp
    src/config/preferences.cpp
    src/device_info.cpp
    src/net/control_plane.cpp
    src/net/loopback_auth.cpp
    src/net/upstream_pool.cpp
    src/net/upstream_call.cpp
    src/account/cancel_worker.cpp
    src/io/wav_io.cpp
    src/io/image_io.cpp
    src/io/output.cpp
    src/progress/progress_bar.cpp
    src/repl/repl.cpp
    src/util/term.cpp
)

add_library(wally_core STATIC ${WALLY_SOURCES})

if(WIN32)
    target_compile_definitions(wally_core PUBLIC WALLY_NO_LINENOISE=1)
    if(MSVC)
        target_compile_options(wally_core PUBLIC
            "/FI${CMAKE_CURRENT_SOURCE_DIR}/src/windows_proto_compat.h")
    else()
        target_compile_options(wally_core PUBLIC
            -include "${CMAKE_CURRENT_SOURCE_DIR}/src/windows_proto_compat.h")
    endif()
else()
    target_sources(wally_core PRIVATE third_party/linenoise/linenoise.c)
    set_source_files_properties(third_party/linenoise/linenoise.c PROPERTIES COMPILE_OPTIONS "-w")
endif()

target_include_directories(wally_core PUBLIC
    "${CMAKE_CURRENT_SOURCE_DIR}/include"
    "${CMAKE_CURRENT_SOURCE_DIR}/src"
    "${CMAKE_CURRENT_SOURCE_DIR}/third_party"
    "${CMAKE_CURRENT_SOURCE_DIR}/third_party/CLI11"
    "${CMAKE_CURRENT_SOURCE_DIR}/third_party/linenoise"
)

# Dev endpoint bake. The values come from the ENVIRONMENT of the configure
# step, never from -D flags: nothing endpoint-shaped lands on the command
# line, in CMakeCache.txt, or in this log (only ON/off is printed). The
# generated header lives in the build tree, which is gitignored — no endpoint
# ever enters committed source. Empty env (the normal case) generates empty
# macros and the production resolution order is untouched.
set(WALLY_BAKED_CONSOLE_API_URL "$ENV{WALLY_BAKED_CONSOLE_API_URL}")
set(WALLY_BAKED_CONSOLE_WEB_ORIGIN "$ENV{WALLY_BAKED_CONSOLE_WEB_ORIGIN}")
if(WALLY_BAKED_CONSOLE_API_URL)
    message(STATUS "wally: dev endpoint bake ON (values withheld from log)")
else()
    message(STATUS "wally: dev endpoint bake off (production defaults)")
endif()
configure_file(
    "${CMAKE_CURRENT_SOURCE_DIR}/src/account/baked_endpoints.h.in"
    "${CMAKE_CURRENT_BINARY_DIR}/generated/account/baked_endpoints.h"
    @ONLY
)
target_include_directories(wally_core PUBLIC "${CMAKE_CURRENT_BINARY_DIR}/generated")

# The Anthropic translator and the JetBrains proxy need an HTTP client and a
# JSON reader. Both used to arrive from the SDK's own source build; a kit
# consumer never configures that build, so they are fetched here.
#
# Fetched rather than vendored on purpose. Checking cpp-httplib in would drop
# thirty thousand lines of somebody else's code into this repo, where the
# security scanner reads it as ours and flags the http:// URLs an HTTP client
# is obliged to construct.
include(FetchContent)
set(HTTPLIB_REQUIRE_OPENSSL OFF CACHE BOOL "" FORCE)
set(JSON_BuildTests OFF CACHE BOOL "" FORCE)
FetchContent_Declare(cpp_httplib
    GIT_REPOSITORY https://github.com/yhirose/cpp-httplib.git
    GIT_TAG v0.46.1
    GIT_SHALLOW TRUE)
FetchContent_Declare(nlohmann_json
    GIT_REPOSITORY https://github.com/nlohmann/json.git
    GIT_TAG v3.11.3
    GIT_SHALLOW TRUE)
FetchContent_MakeAvailable(cpp_httplib nlohmann_json)

find_package(Threads REQUIRED)
target_link_libraries(wally_core PUBLIC rac_commons Threads::Threads)
target_link_libraries(wally_core PRIVATE httplib::httplib nlohmann_json::nlohmann_json)
if(WIN32)
    # The console client uses the Windows system HTTP stack so source builds and
    # release archives do not need curl/OpenSSL DLLs beside wally.exe.
    target_link_libraries(wally_core PRIVATE winhttp)
else()
    find_package(CURL REQUIRED)
    target_link_libraries(wally_core PRIVATE CURL::libcurl)
endif()
target_compile_definitions(wally_core PUBLIC
    WALLY_VERSION="${PROJECT_VERSION}"
    WALLY_PINNED_SDK_VERSION="${WALLY_PINNED_SDK_VERSION}"
    RAC_HAVE_PROTOBUF=1
    CLI11_HAS_CODECVT=0)
wally_define_engine_macros(wally_core)
if(RunAnywhere_HAS_RAG)
    target_compile_definitions(wally_core PUBLIC RAC_HAVE_RAG=1)
endif()
if(RunAnywhere_HAS_SERVER)
    target_compile_definitions(wally_core PUBLIC WALLY_HAS_SERVER=1)
endif()

# Proto types are the SOT. The kit ships the *.pb.h that commons was compiled
# with (same protoc, same isolated runtime). WALLY never runs protoc and never
# compiles *.pb.cc — those objects already live in librac_commons.a.
if(NOT RunAnywhere_PROTO_INCLUDE_DIR
   OR NOT EXISTS "${RunAnywhere_PROTO_INCLUDE_DIR}/model_types.pb.h"
   OR NOT EXISTS "${RunAnywhere_PROTO_INCLUDE_DIR}/schema_lock.h")
    message(FATAL_ERROR
        "RunAnywhere kit is missing generated proto headers "
        "(RunAnywhere_PROTO_INCLUDE_DIR=${RunAnywhere_PROTO_INCLUDE_DIR}). "
        "Rebuild the kit with package-cpp-desktop.")
endif()
# Include path + google=runanywhere_internal come from RunAnywhere::commons.

if(APPLE)
    # SecItem, for the credential a JetBrains IDE reads its provider key from.
    target_link_libraries(wally_core PRIVATE "-framework Security" "-framework CoreFoundation")
    target_link_libraries(wally_core PUBLIC "-framework IOKit" "-framework CoreFoundation")
endif()

if(CMAKE_CXX_COMPILER_ID MATCHES "Clang|GNU")
    target_compile_options(wally_core PRIVATE -Wall -Wextra)
endif()

add_executable(wally src/main.cpp)
target_link_libraries(wally PRIVATE wally_core)

if(MINGW)
    target_link_options(wally PRIVATE -static-libgcc -static-libstdc++ -static)
endif()

# On Apple the shipped product is the Swift MLX host at build/wally (see the
# wally-mlx target below); the pure-C++ build of this target is an intermediate
# the Swift host links, not a product. Name it wally-cxx so `cmake --build`
# cannot clobber the fat Apple `wally`, and by default keep it out of the build
# root so a normal build surfaces only `wally`. Set WALLY_APPLE_CXX_BINARY=ON to
# get the standalone C++ binary at build/wally-cxx for a fast C++-only loop.
option(WALLY_APPLE_MLX_HOST
    "On Apple, link the Swift MLX host as build/wally (the product CLI)" ON)
option(WALLY_APPLE_CXX_BINARY
    "On Apple, emit the standalone C++ wally-cxx binary in the build root" OFF)

set(_wally_runtime_dir "${CMAKE_BINARY_DIR}")
if(APPLE)
    set_target_properties(wally PROPERTIES OUTPUT_NAME "wally-cxx")
    if(WALLY_APPLE_MLX_HOST AND NOT WALLY_APPLE_CXX_BINARY)
        set(_wally_runtime_dir "${CMAKE_BINARY_DIR}/cxx-host")
    endif()
endif()

set_target_properties(wally PROPERTIES RUNTIME_OUTPUT_DIRECTORY "${_wally_runtime_dir}")
if(APPLE)
    set_target_properties(wally PROPERTIES
        BUILD_RPATH "${RunAnywhere_THIRD_PARTY_DIR}"
        INSTALL_RPATH "@loader_path/../lib")
elseif(UNIX)
    set_target_properties(wally PROPERTIES
        BUILD_RPATH "${RunAnywhere_THIRD_PARTY_DIR}"
        INSTALL_RPATH "\$ORIGIN/../lib")
endif()
wally_stage_windows_runtime_dlls(wally)

install(TARGETS wally RUNTIME DESTINATION bin)

# Apple product binary is the Swift MLX host (`build/wally`), not `wally-cxx`.
# Users still type `wally`; llama.cpp / ONNX / Sherpa register from C++ bootstrap
# and MLX from the Swift entry. Turn WALLY_APPLE_MLX_HOST (declared above) off
# only for a C++-only compile loop.
if(APPLE AND WALLY_APPLE_MLX_HOST)
    if(NOT WALLY_SDK_ROOT)
        message(FATAL_ERROR "WALLY_SDK_ROOT is empty; cannot build the MLX host")
    endif()
    add_custom_target(wally-mlx ALL
        COMMAND "${CMAKE_COMMAND}" -E env
            "WALLY_SDK_KIT=${WALLY_SDK_ROOT}"
            bash "${CMAKE_SOURCE_DIR}/scripts/build/build-mlx.sh" "${CMAKE_BINARY_DIR}"
        DEPENDS wally
        WORKING_DIRECTORY "${CMAKE_SOURCE_DIR}"
        COMMENT "Swift MLX host → ${CMAKE_BINARY_DIR}/wally"
        VERBATIM)
endif()

include(CTest)
if(BUILD_TESTING)
    add_subdirectory(tests)
endif()
