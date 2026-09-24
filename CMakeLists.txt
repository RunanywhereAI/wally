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
# where the formula, Cargo.toml or the Swift package pin has drifted from
# versions.toml.

# The application is Rust (Cargo.toml, src/). C++ remains only for the kit's own
# headers and the link probe, so the kit's C++20 requirement still applies to
# anything compiled against it.
set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)

if(NOT CMAKE_BUILD_TYPE AND NOT CMAKE_CONFIGURATION_TYPES)
    set(CMAKE_BUILD_TYPE Release)
endif()

list(APPEND CMAKE_MODULE_PATH "${CMAKE_CURRENT_SOURCE_DIR}/cmake")
include(RunAnywhereSDK)
find_package(Threads REQUIRED)

# Dev endpoint bake. The values come from the ENVIRONMENT of the configure
# step, never from -D flags: nothing endpoint-shaped lands on the command
# line, in CMakeCache.txt, or in this log (only ON/off is printed). They reach
# the Rust crate through generated/wally-build.env, which lives in the build
# tree and is gitignored — no endpoint ever enters committed source. Empty env
# (the normal case) bakes nothing and the production resolution order is
# untouched.
set(WALLY_BAKED_CONSOLE_API_URL "$ENV{WALLY_BAKED_CONSOLE_API_URL}")
set(WALLY_BAKED_CONSOLE_WEB_ORIGIN "$ENV{WALLY_BAKED_CONSOLE_WEB_ORIGIN}")
# WALLY_CHANNEL states what is being built. A release job that means to build a
# dev bottle but whose endpoint variables are unset would otherwise produce a
# `-dev` archive that quietly defaults to production, because an empty bake is
# indistinguishable from an ordinary production build (wally #87). Declaring the
# intent is what lets that be caught here rather than by a person wondering why
# nightly is billing them.
set(WALLY_CHANNEL "$ENV{WALLY_CHANNEL}")
if(WALLY_CHANNEL STREQUAL "dev")
    if(NOT WALLY_BAKED_CONSOLE_API_URL OR NOT WALLY_BAKED_CONSOLE_WEB_ORIGIN)
        message(FATAL_ERROR
            "WALLY_CHANNEL=dev needs both WALLY_BAKED_CONSOLE_API_URL and "
            "WALLY_BAKED_CONSOLE_WEB_ORIGIN in the configure environment. "
            "Without them this builds a production binary under a -dev name.")
    endif()
endif()
# Half a pair is a configuration mistake in either direction: an API with no
# approval origin cannot complete a login, and an origin with no API is trusted
# for nothing.
if(WALLY_BAKED_CONSOLE_API_URL AND NOT WALLY_BAKED_CONSOLE_WEB_ORIGIN)
    message(FATAL_ERROR "WALLY_BAKED_CONSOLE_API_URL is set without "
                        "WALLY_BAKED_CONSOLE_WEB_ORIGIN; `wally login` could not "
                        "trust any approval page.")
endif()
if(WALLY_BAKED_CONSOLE_WEB_ORIGIN AND NOT WALLY_BAKED_CONSOLE_API_URL)
    message(FATAL_ERROR "WALLY_BAKED_CONSOLE_WEB_ORIGIN is set without "
                        "WALLY_BAKED_CONSOLE_API_URL; the origin is paired with "
                        "an API that was not baked.")
endif()
foreach(_baked_url "${WALLY_BAKED_CONSOLE_API_URL}" "${WALLY_BAKED_CONSOLE_WEB_ORIGIN}")
    if(_baked_url AND NOT _baked_url MATCHES "^https?://[^/ ]+")
        message(FATAL_ERROR
            "a baked endpoint is not an http(s) origin. Values are withheld from "
            "this log; check the configure environment.")
    endif()
endforeach()
if(WALLY_BAKED_CONSOLE_API_URL)
    message(STATUS "wally: dev endpoint bake ON (values withheld from log)")
else()
    message(STATUS "wally: dev endpoint bake off (production defaults)")
endif()

# The model a harness launch falls back to when the reader passes no -m and set
# no default. Set here so the id lives in the build, not the source; a developer
# overrides it with -DWALLY_DEFAULT_MODEL_ID=<id>, and this is the default when
# they do not.
set(WALLY_DEFAULT_MODEL_ID "glm-5.3-flash" CACHE STRING
    "Default model a harness launch uses when given no -m")

# Proto types are the SOT. build.rs generates the Rust types from the kit's own
# share/runanywhere/idl after checking its SCHEMA_LOCK hash against the pin;
# wally never runs protoc. The C++ headers the kit also ships are unused here but
# still required, so a kit built without them is rejected as before.
if(NOT RunAnywhere_PROTO_INCLUDE_DIR
   OR NOT EXISTS "${RunAnywhere_PROTO_INCLUDE_DIR}/model_types.pb.h"
   OR NOT EXISTS "${RunAnywhere_PROTO_INCLUDE_DIR}/schema_lock.h")
    message(FATAL_ERROR
        "RunAnywhere kit is missing generated proto headers "
        "(RunAnywhere_PROTO_INCLUDE_DIR=${RunAnywhere_PROTO_INCLUDE_DIR}). "
        "Rebuild the kit with package-cpp-desktop.")
endif()

include(WallyRust)

# On Apple the shipped product is the Swift MLX host at build/wally (see the
# wally-mlx target below); the plain cargo binary is an intermediate. Name it
# wally-cxx so `cmake --build` cannot clobber the fat Apple `wally`, and by
# default keep it out of the build root so a normal build surfaces only
# `wally`. Set WALLY_APPLE_CXX_BINARY=ON to get it at build/wally-cxx for a fast
# loop without the Swift host. (The name predates the Rust port; scripts and
# docs still use it.)
option(WALLY_APPLE_MLX_HOST
    "On Apple, link the Swift MLX host as build/wally (the product CLI)" ON)
option(WALLY_APPLE_CXX_BINARY
    "On Apple, emit the plain (non-Swift) wally-cxx binary in the build root" OFF)

set(_wally_runtime_dir "${CMAKE_BINARY_DIR}")
set(_wally_product_name "wally${CMAKE_EXECUTABLE_SUFFIX}")
if(APPLE)
    set(_wally_product_name "wally-cxx")
    if(WALLY_APPLE_MLX_HOST AND NOT WALLY_APPLE_CXX_BINARY)
        set(_wally_runtime_dir "${CMAKE_BINARY_DIR}/cxx-host")
    endif()
endif()
set(WALLY_PRODUCT_BINARY "${_wally_runtime_dir}/${_wally_product_name}")

add_custom_target(wally ALL
    COMMAND "${CMAKE_COMMAND}" -E make_directory "${_wally_runtime_dir}"
    COMMAND "${CMAKE_COMMAND}" -E copy_if_different "${WALLY_CARGO_BIN}" "${WALLY_PRODUCT_BINARY}"
    BYPRODUCTS "${WALLY_PRODUCT_BINARY}"
    DEPENDS wally_cargo
    COMMENT "wally → ${WALLY_PRODUCT_BINARY}"
    VERBATIM)
wally_stage_windows_runtime_dlls(wally "${_wally_runtime_dir}")
wally_bundle_product_dlls(wally "${_wally_runtime_dir}")

install(PROGRAMS "${WALLY_PRODUCT_BINARY}" DESTINATION bin)

# Apple product binary is the Swift MLX host (`build/wally`), not `wally-cxx`.
# Users still type `wally`; llama.cpp / ONNX / Sherpa register from the Rust
# bootstrap and MLX from the Swift entry. Turn WALLY_APPLE_MLX_HOST (declared
# above) off only for a loop without the Swift host.
if(APPLE AND WALLY_APPLE_MLX_HOST)
    if(NOT WALLY_SDK_ROOT)
        message(FATAL_ERROR "WALLY_SDK_ROOT is empty; cannot build the MLX host")
    endif()
    add_custom_target(wally-mlx ALL
        COMMAND "${CMAKE_COMMAND}" -E env
            "WALLY_SDK_KIT=${WALLY_SDK_ROOT}"
            "WALLY_RUST_STATICLIB=${WALLY_RUST_STATICLIB}"
            bash "${CMAKE_SOURCE_DIR}/scripts/build/build-mlx.sh" "${CMAKE_BINARY_DIR}"
        DEPENDS wally
        WORKING_DIRECTORY "${CMAKE_SOURCE_DIR}"
        COMMENT "Swift MLX host → ${CMAKE_BINARY_DIR}/wally"
        VERBATIM)
endif()

# `ctest --test-dir build` stays the CI bar: it runs the whole Rust test suite
# (every ported C++ case plus the golden CLI corpus) with the kit linked the way
# the product is, and the kit's runtime directories on PATH so Windows test
# executables find their DLLs.
include(CTest)
if(BUILD_TESTING)
    if(WIN32)
        set(_wally_test_path "${WALLY_RUNTIME_PATH_DIRS};$ENV{PATH}")
        string(REPLACE ";" "\\;" _wally_test_path "${_wally_test_path}")
    else()
        string(REPLACE ";" ":" _wally_test_path "${WALLY_RUNTIME_PATH_DIRS}")
        set(_wally_test_path "${_wally_test_path}:$ENV{PATH}")
    endif()
    add_test(NAME wally_rust_tests
        COMMAND "${CMAKE_COMMAND}" -E env
            "WALLY_BUILD_ENV=${WALLY_BUILD_ENV_FILE}"
            "CARGO_TARGET_DIR=${WALLY_CARGO_TARGET_DIR}"
            "${WALLY_CARGO}" test ${_wally_cargo_profile_flag} --locked
                --manifest-path "${CMAKE_SOURCE_DIR}/Cargo.toml" --no-fail-fast
        WORKING_DIRECTORY "${CMAKE_SOURCE_DIR}")
    set_tests_properties(wally_rust_tests PROPERTIES
        ENVIRONMENT "PATH=${_wally_test_path}"
        TIMEOUT 3600)
endif()
