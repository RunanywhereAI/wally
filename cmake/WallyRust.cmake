# Building the Rust crate against the kit that RunAnywhereSDK.cmake resolved.
#
# CMake stays the single owner of the kit: discovery, pin/schema checks, the
# per-platform link closure (whole-archive registrars, GNU link groups, Windows
# import fix-ups, Apple frameworks, the server's OpenSSL, overlays) and runtime
# staging. Cargo owns compiling Rust. The two meet in two files, both in the
# build tree:
#
#   generated/wally-build.env  written here at configure time; build.rs reads it
#                              for the kit IDL dir, the engine capability flags,
#                              the default model and (dev channel only) the
#                              baked console endpoints — values never go on a
#                              command line.
#   the CMake file API reply   the exact link line of `wally_link_probe`, an
#                              executable that is configured but never built.
#                              build.rs turns its link fragments into linker
#                              arguments, so what cargo links (the product
#                              binary on Windows/Linux, every test) carries the
#                              same kit closure a CMake target would.
#
# Products: on Windows and Linux the cargo-built `wally` is the product and is
# copied to the build root next to its runtime DLLs. On Apple the product is the
# Swift MLX host (scripts/build/build-mlx.sh), linked against this crate's
# static library; the cargo binary is the `wally-cxx` intermediate.

# A multi-config generator (Xcode, Visual Studio) leaves CMAKE_BUILD_TYPE empty
# at configure time -- the config is only chosen at build time, per `--config`.
# The cargo profile and wally_link_probe's LINK_PROBE_CONFIG below are both
# picked once, here, from CMAKE_BUILD_TYPE, so a multi-config generator would
# silently build cargo in release while `cmake --build --config Debug` links
# against a probe configured for no config at all. Every CI/release job already
# pins a single-config generator (Ninja); fail fast instead of shipping that
# mismatch.
get_property(_wally_multi_config GLOBAL PROPERTY GENERATOR_IS_MULTI_CONFIG)
if(_wally_multi_config)
    message(FATAL_ERROR
        "wally's Cargo/CMake bridge requires a single-config generator "
        "(e.g. -G Ninja); ${CMAKE_GENERATOR} is multi-config and "
        "CMAKE_BUILD_TYPE has no effect on the Cargo profile or link-probe "
        "configuration chosen here.")
endif()

find_program(WALLY_CARGO NAMES cargo HINTS "$ENV{CARGO_HOME}/bin" "$ENV{HOME}/.cargo/bin"
             "$ENV{USERPROFILE}/.cargo/bin")
if(NOT WALLY_CARGO)
    message(FATAL_ERROR "cargo not found. Install Rust with rustup (https://rustup.rs); "
                        "rust-toolchain.toml pins the version.")
endif()

# The probe: the same link inputs the C++ `wally` executable had, minus the
# application code (which is now the Rust crate).
add_executable(wally_link_probe EXCLUDE_FROM_ALL cmake/wally_link_probe.cpp)
target_link_libraries(wally_link_probe PRIVATE rac_commons Threads::Threads)
wally_define_engine_macros(wally_link_probe)
if(APPLE)
    target_link_libraries(wally_link_probe PRIVATE "-framework IOKit" "-framework CoreFoundation")
    set_target_properties(wally_link_probe PROPERTIES BUILD_RPATH "${RunAnywhere_THIRD_PARTY_DIR}")
elseif(UNIX)
    set_target_properties(wally_link_probe PROPERTIES BUILD_RPATH "${RunAnywhere_THIRD_PARTY_DIR}")
endif()

# The kit's librac_server.a embeds cpp-httplib, compiled with whatever
# compression and platform networking its build machine offered (brotli, zstd,
# and on Apple CFNetwork/Security), but its imported target does not declare
# them. The C++ build linked them transitively through its own FetchContent'd
# httplib target, which looked for each "if available". Linking that same
# target (same tag, same options) into the probe reproduces that closure on
# every platform instead of guessing it per OS. httplib is header-only; nothing
# of it is compiled here.
include(FetchContent)
set(HTTPLIB_REQUIRE_OPENSSL OFF CACHE BOOL "" FORCE)
FetchContent_Declare(cpp_httplib
    GIT_REPOSITORY https://github.com/yhirose/cpp-httplib.git
    GIT_TAG v0.46.1
    GIT_SHALLOW TRUE)
FetchContent_MakeAvailable(cpp_httplib)
target_link_libraries(wally_link_probe PRIVATE httplib::httplib)

# Ask for the codemodel reply; CMake writes it at the end of this same
# generation. Requires cmake_file_api() (3.27+, enforced by the
# cmake_minimum_required at the top of CMakeLists.txt): writing the query file
# directly instead, as CMake < 3.27 requires, only takes effect on the next
# configure, so the first cargo build would run before the reply exists.
cmake_file_api(QUERY API_VERSION 1 CODEMODEL 2)

set(_wally_caps "")
foreach(_cap LLAMACPP ONNX SHERPA MLX CLOUD RAG SERVER)
    if(RunAnywhere_HAS_${_cap})
        string(APPEND _wally_caps "HAS_${_cap}=1\n")
    else()
        string(APPEND _wally_caps "HAS_${_cap}=0\n")
    endif()
endforeach()
if(TARGET RunAnywhere::neurt)
    string(APPEND _wally_caps "HAS_NEURT=1\n")
else()
    string(APPEND _wally_caps "HAS_NEURT=0\n")
endif()
if(TARGET RunAnywhere::qhexrt)
    string(APPEND _wally_caps "HAS_QHEXRT=1\n")
else()
    string(APPEND _wally_caps "HAS_QHEXRT=0\n")
endif()

set(WALLY_BUILD_ENV_FILE "${CMAKE_BINARY_DIR}/generated/wally-build.env")
file(WRITE "${WALLY_BUILD_ENV_FILE}.in"
"# Generated by cmake/WallyRust.cmake; read by build.rs. Build tree only.
KIT_ROOT=${WALLY_SDK_ROOT}
KIT_IDL_DIR=${RunAnywhere_IDL_DIR}
KIT_THIRD_PARTY_DIR=${RunAnywhere_THIRD_PARTY_DIR}
${_wally_caps}DEFAULT_MODEL_ID=${WALLY_DEFAULT_MODEL_ID}
BAKED_CONSOLE_API_URL=${WALLY_BAKED_CONSOLE_API_URL}
BAKED_CONSOLE_WEB_ORIGIN=${WALLY_BAKED_CONSOLE_WEB_ORIGIN}
CMAKE_FILE_API_REPLY=${CMAKE_BINARY_DIR}/.cmake/api/v1/reply
LINK_PROBE_TARGET=wally_link_probe
LINK_PROBE_CONFIG=${CMAKE_BUILD_TYPE}
")
# Only touch the file when it changes, so cargo does not rebuild on every configure.
configure_file("${WALLY_BUILD_ENV_FILE}.in" "${WALLY_BUILD_ENV_FILE}" COPYONLY)

if(CMAKE_BUILD_TYPE STREQUAL "Debug")
    set(_wally_cargo_profile_flag "")
    set(_wally_cargo_profile_dir "debug")
else()
    set(_wally_cargo_profile_flag "--release")
    set(_wally_cargo_profile_dir "release")
endif()
set(WALLY_CARGO_TARGET_DIR "${CMAKE_BINARY_DIR}/cargo")
set(WALLY_CARGO_OUT_DIR "${WALLY_CARGO_TARGET_DIR}/${_wally_cargo_profile_dir}")
if(WIN32)
    set(WALLY_RUST_STATICLIB "${WALLY_CARGO_OUT_DIR}/wally.lib")
    set(WALLY_CARGO_BIN "${WALLY_CARGO_OUT_DIR}/wally.exe")
else()
    set(WALLY_RUST_STATICLIB "${WALLY_CARGO_OUT_DIR}/libwally.a")
    set(WALLY_CARGO_BIN "${WALLY_CARGO_OUT_DIR}/wally")
endif()

# The environment of every cargo run CMake starts (this build and the CTest
# runs in CMakeLists.txt). With MSVC it names the linker CMake found: rustc
# otherwise runs whichever `link.exe` is first on PATH, and in a Git Bash step
# (CI's shell) that is coreutils' /usr/bin/link, not MSVC's.
set(WALLY_CARGO_ENV
    "WALLY_BUILD_ENV=${WALLY_BUILD_ENV_FILE}"
    "CARGO_TARGET_DIR=${WALLY_CARGO_TARGET_DIR}")
if(MSVC AND CMAKE_LINKER)
    list(APPEND WALLY_CARGO_ENV
        "CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER=${CMAKE_LINKER}"
        "CARGO_TARGET_AARCH64_PC_WINDOWS_MSVC_LINKER=${CMAKE_LINKER}")
endif()

# cargo decides what is stale; this target always asks it.
add_custom_target(wally_cargo ALL
    COMMAND "${CMAKE_COMMAND}" -E env
        ${WALLY_CARGO_ENV}
        "WALLY_NATIVE_LINK_ARGS_OUT=${CMAKE_BINARY_DIR}/wally-native-link-args.txt"
        "${WALLY_CARGO}" build ${_wally_cargo_profile_flag} --locked
            --manifest-path "${CMAKE_SOURCE_DIR}/Cargo.toml" --lib --bin wally
    BYPRODUCTS "${WALLY_RUST_STATICLIB}" "${WALLY_CARGO_BIN}"
               "${CMAKE_BINARY_DIR}/wally-native-link-args.txt"
    WORKING_DIRECTORY "${CMAKE_SOURCE_DIR}"
    COMMENT "cargo build (wally crate)"
    USES_TERMINAL
    VERBATIM)

# The directory CTest and the Windows DLL staging use for cargo's executables.
set(WALLY_RUNTIME_PATH_DIRS "${RunAnywhere_THIRD_PARTY_DIR}")
if(DEFINED RunAnywhere_LIBRARY_DIR)
    list(APPEND WALLY_RUNTIME_PATH_DIRS "${RunAnywhere_LIBRARY_DIR}/../bin")
endif()
