# Consumes a packaged C++ desktop kit via find_package(RunAnywhere).
# The SDK is never compiled as a subdirectory or FetchContent'd.
#
#   CMAKE_PREFIX_PATH=<kit prefix>     # dist/cpp-desktop-<os>-<arch>
#   or RUNANYWHERE_ROOT=<kit prefix>
#   or WALLY_SDK_KIT=<kit prefix>
#
# WALLY_SDK_DIR is accepted only as an alias for a *kit* prefix (must contain
# lib/cmake/RunAnywhere/RunAnywhereConfig.cmake). Pointing it at the monorepo
# source tree is a hard error.

set(WALLY_SDK_KIT "" CACHE PATH "Path to a staged RunAnywhere C++ desktop kit")
set(WALLY_SDK_DIR "" CACHE PATH "Deprecated alias for WALLY_SDK_KIT (kit prefix, not source)")
set(WALLY_SDK_VERSION "" CACHE STRING "EXACT find_package version; default from cmake/sdk-pin.cmake")
option(WALLY_AUTO_FETCH_KIT "Download the pinned kit into .deps/kit when none is on CMAKE_PREFIX_PATH" ON)

if(WALLY_SDK_DIR AND NOT WALLY_SDK_KIT)
    set(WALLY_SDK_KIT "${WALLY_SDK_DIR}")
endif()

include("${CMAKE_CURRENT_LIST_DIR}/sdk-pin.cmake")
if(WALLY_SDK_VERSION AND NOT WALLY_SDK_VERSION STREQUAL WALLY_PINNED_SDK_VERSION)
    message(FATAL_ERROR
        "WALLY_SDK_VERSION=${WALLY_SDK_VERSION} does not match "
        "WALLY_PINNED_SDK_VERSION=${WALLY_PINNED_SDK_VERSION} in cmake/sdk-pin.cmake. "
        "Bump the pin (and kit checksums) instead of overriding the version.")
endif()
if(NOT WALLY_SDK_VERSION)
    set(WALLY_SDK_VERSION "${WALLY_PINNED_SDK_VERSION}")
endif()

if(WALLY_SDK_KIT)
    if(EXISTS "${WALLY_SDK_KIT}/CMakeLists.txt" AND NOT EXISTS
       "${WALLY_SDK_KIT}/lib/cmake/RunAnywhere/RunAnywhereConfig.cmake")
        message(FATAL_ERROR
            "WALLY_SDK_KIT/WALLY_SDK_DIR points at SDK source (${WALLY_SDK_KIT}). "
            "Build the C++ desktop kit first:\n"
            "  cmake --preset cpp-desktop-macos-arm64 && "
            "cmake --build --preset cpp-desktop-macos-arm64 --target package-cpp-desktop\n"
            "then pass -DWALLY_SDK_KIT=<sdks>/dist/cpp-desktop-<os>-<arch>")
    endif()
    list(PREPEND CMAKE_PREFIX_PATH "${WALLY_SDK_KIT}")
endif()
if(DEFINED ENV{RUNANYWHERE_ROOT} AND NOT WALLY_SDK_KIT)
    list(PREPEND CMAKE_PREFIX_PATH "$ENV{RUNANYWHERE_ROOT}")
endif()

# An IDE opening this tree for the first time runs a bare `cmake -S . -B ...`
# with no kit on CMAKE_PREFIX_PATH, so find_package would hard-fail before the
# person ever saw a build. Fetch the pinned kit into the gitignored .deps/kit
# instead. This is the same scripts/build/fetch-kit.sh CI runs -- pinned version,
# checksum verified -- never a build from SDK source.
find_package(RunAnywhere ${WALLY_SDK_VERSION} EXACT QUIET CONFIG)

if(NOT RunAnywhere_FOUND AND WALLY_AUTO_FETCH_KIT)
    if(APPLE AND CMAKE_SYSTEM_PROCESSOR MATCHES "arm64|aarch64")
        set(_wally_kit_platform "macos-arm64")
    elseif(WIN32 AND CMAKE_SYSTEM_PROCESSOR MATCHES "ARM64|arm64|aarch64")
        set(_wally_kit_platform "windows-arm64")
    elseif(WIN32)
        set(_wally_kit_platform "windows-x64")
    elseif(UNIX AND CMAKE_SYSTEM_PROCESSOR MATCHES "x86_64|AMD64")
        set(_wally_kit_platform "linux-x64")
    endif()

    find_program(WALLY_BASH_EXECUTABLE NAMES bash)
    if(_wally_kit_platform AND WALLY_BASH_EXECUTABLE)
        set(_wally_auto_kit "${CMAKE_SOURCE_DIR}/.deps/kit")
        message(STATUS
            "wally: no RunAnywhere kit on CMAKE_PREFIX_PATH; fetching pinned "
            "${WALLY_SDK_VERSION} (${_wally_kit_platform}) into .deps/kit")
        execute_process(
            COMMAND "${WALLY_BASH_EXECUTABLE}"
                    "${CMAKE_SOURCE_DIR}/scripts/build/fetch-kit.sh"
                    "${_wally_kit_platform}" "${_wally_auto_kit}"
            RESULT_VARIABLE _wally_fetch_result)
        if(_wally_fetch_result EQUAL 0)
            list(PREPEND CMAKE_PREFIX_PATH "${_wally_auto_kit}")
            find_package(RunAnywhere ${WALLY_SDK_VERSION} EXACT QUIET CONFIG)
        else()
            message(WARNING
                "wally: fetch-kit.sh failed (${_wally_fetch_result}). It needs the "
                "GitHub CLI signed in: `gh auth login`. Or pass a kit yourself with "
                "-DWALLY_SDK_KIT=<prefix>.")
        endif()
    endif()
endif()

if(NOT RunAnywhere_FOUND)
    find_package(RunAnywhere ${WALLY_SDK_VERSION} EXACT REQUIRED CONFIG)
endif()

# Proto is the SOT across the two repos. The kit stamps SCHEMA_LOCK into
# find_package vars; this pin must match. Do not run protoc in WALLY to "fix"
# a mismatch — bump the pin after consuming a new kit.
if(NOT RunAnywhere_IDL_SCHEMA_SHA256)
    message(FATAL_ERROR
        "RunAnywhere kit is missing IDL schema metadata "
        "(RunAnywhere_IDL_SCHEMA_SHA256). Rebuild with package-cpp-desktop.")
endif()
if(NOT RunAnywhere_IDL_SCHEMA_SHA256 STREQUAL WALLY_PINNED_IDL_SCHEMA_SHA256
   OR NOT RunAnywhere_IDL_VERSION STREQUAL WALLY_PINNED_IDL_VERSION
   OR NOT RunAnywhere_IDL_PROTOC_VERSION STREQUAL WALLY_PINNED_IDL_PROTOC_VERSION)
    message(FATAL_ERROR
        "RunAnywhere kit IDL does not match cmake/sdk-pin.cmake.\n"
        "  kit:  ${RunAnywhere_IDL_VERSION} / ${RunAnywhere_IDL_SCHEMA_SHA256} / protoc ${RunAnywhere_IDL_PROTOC_VERSION}\n"
        "  pin:  ${WALLY_PINNED_IDL_VERSION} / ${WALLY_PINNED_IDL_SCHEMA_SHA256} / protoc ${WALLY_PINNED_IDL_PROTOC_VERSION}\n"
        "Consume the kit that matches this pin, or update the pin after a schema bump. "
        "WALLY must not run protoc.")
endif()

set(WALLY_SDK_ROOT "${RunAnywhere_INCLUDE_DIR}/.." CACHE INTERNAL "")
set(WALLY_IDL_DIR "${RunAnywhere_IDL_DIR}" CACHE INTERNAL "")
set(WALLY_PROTO_INCLUDE_DIR "${RunAnywhere_PROTO_INCLUDE_DIR}" CACHE INTERNAL "")

if(NOT TARGET RunAnywhere::commons)
    message(FATAL_ERROR "find_package(RunAnywhere) did not import RunAnywhere::commons")
endif()

# MSVC link.exe cannot consume a DLL (LNK1107). Older kits globbed
# third_party/onnxruntime.dll into INTERFACE_LINK_LIBRARIES. Drop any
# .dll from the imported target and, when present, link the import lib.
if(WIN32)
    get_target_property(_wally_ra_ifaces RunAnywhere::commons INTERFACE_LINK_LIBRARIES)
    if(_wally_ra_ifaces)
        set(_wally_ra_kept "")
        foreach(_lib IN LISTS _wally_ra_ifaces)
            if(_lib MATCHES "\\.[Dd][Ll][Ll]$")
                continue()
            endif()
            list(APPEND _wally_ra_kept "${_lib}")
        endforeach()
        set_property(TARGET RunAnywhere::commons PROPERTY INTERFACE_LINK_LIBRARIES "${_wally_ra_kept}")
    endif()
    if(DEFINED RunAnywhere_LIBRARY_DIR AND EXISTS "${RunAnywhere_LIBRARY_DIR}/onnxruntime.lib")
        get_target_property(_wally_ra_ifaces RunAnywhere::commons INTERFACE_LINK_LIBRARIES)
        set(_wally_ra_ifaces "${_wally_ra_ifaces}")
        if(NOT _wally_ra_ifaces MATCHES "onnxruntime\\.lib")
            set_property(TARGET RunAnywhere::commons APPEND PROPERTY
                INTERFACE_LINK_LIBRARIES "${RunAnywhere_LIBRARY_DIR}/onnxruntime.lib")
        endif()
    endif()
    if(DEFINED RunAnywhere_LIBRARY_DIR AND EXISTS "${RunAnywhere_LIBRARY_DIR}/libcurl.lib")
        get_target_property(_wally_ra_ifaces RunAnywhere::commons INTERFACE_LINK_LIBRARIES)
        set(_wally_ra_ifaces "${_wally_ra_ifaces}")
        if(NOT _wally_ra_ifaces MATCHES "libcurl\\.lib")
            set_property(TARGET RunAnywhere::commons APPEND PROPERTY
                INTERFACE_LINK_LIBRARIES "${RunAnywhere_LIBRARY_DIR}/libcurl.lib")
        endif()
    endif()
    # Static libcurl/libarchive in 0.20.26 kits omit these Windows imports.
    # Keep them here until a later kit bakes them into SYSTEM_LIBS.
    foreach(_sys IN ITEMS iphlpapi xmllite ole32)
        get_target_property(_wally_ra_ifaces RunAnywhere::commons INTERFACE_LINK_LIBRARIES)
        set(_wally_ra_ifaces "${_wally_ra_ifaces}")
        if(NOT _wally_ra_ifaces MATCHES "${_sys}")
            set_property(TARGET RunAnywhere::commons APPEND PROPERTY
                INTERFACE_LINK_LIBRARIES "${_sys}")
        endif()
    endforeach()
endif()

# GNU ld resolves static archives in a single left-to-right pass, so the kit's
# many interdependent .a files (protobuf<->absl, a backend<->its runtime) leave
# undefined references when their order does not happen to satisfy every
# cross-reference. Wrap the whole imported link interface in a linker group so
# ld re-scans it until resolved. Apple's ld64 and MSVC link.exe do multi-pass
# resolution and need no group; only GNU ld (Linux) does.
if(UNIX AND NOT APPLE)
    # On Linux, sherpa-onnx ships as a shared library in the kit's third_party
    # dir (macOS/Windows bundle it as a static .a). The kit config wires sherpa
    # only through a Windows .lib glob, so the SherpaOnnx* symbols the sherpa
    # backend needs are otherwise unresolved here. Link the .so explicitly; its
    # dir is already on the build rpath (RunAnywhere_THIRD_PARTY_DIR), and the
    # release bottle stages it beside the binary.
    if(DEFINED RunAnywhere_THIRD_PARTY_DIR
       AND EXISTS "${RunAnywhere_THIRD_PARTY_DIR}/libsherpa-onnx-c-api.so")
        set_property(TARGET RunAnywhere::commons APPEND PROPERTY INTERFACE_LINK_LIBRARIES
            "${RunAnywhere_THIRD_PARTY_DIR}/libsherpa-onnx-c-api.so")
    endif()
    # ggml-cpu is built with OpenMP; GNU needs libgomp linked explicitly.
    find_package(OpenMP QUIET)
    # GNU ld resolves static archives in a single left-to-right pass. Wrap the
    # kit's interdependent .a files in a linker group so ld re-scans until
    # resolved — and include librac_commons.a itself, because the backend
    # archives reference symbols (the CPU runtime registry, etc.) that live in
    # commons, which the imported target otherwise places ahead of the group.
    # Apple ld64 and MSVC do multi-pass resolution and need no group.
    get_target_property(_wally_ra_ifaces RunAnywhere::commons INTERFACE_LINK_LIBRARIES)
    set(_wally_grp_head "")
    if(DEFINED RunAnywhere_LIBRARY_DIR AND EXISTS "${RunAnywhere_LIBRARY_DIR}/librac_commons.a")
        list(APPEND _wally_grp_head "${RunAnywhere_LIBRARY_DIR}/librac_commons.a")
    endif()
    set_property(TARGET RunAnywhere::commons PROPERTY INTERFACE_LINK_LIBRARIES
        "-Wl,--start-group" ${_wally_grp_head} ${_wally_ra_ifaces} "-Wl,--end-group")
    if(OpenMP_CXX_FOUND)
        set_property(TARGET RunAnywhere::commons APPEND PROPERTY
            INTERFACE_LINK_LIBRARIES OpenMP::OpenMP_CXX)
    endif()
endif()

# Back-compat name used by the rest of this CMakeLists.
if(NOT TARGET rac_commons)
    add_library(rac_commons ALIAS RunAnywhere::commons)
endif()

function(wally_define_engine_macros target)
    if(RunAnywhere_HAS_LLAMACPP)
        target_compile_definitions(${target} PRIVATE WALLY_HAS_LLAMACPP=1)
    endif()
    if(RunAnywhere_HAS_ONNX)
        target_compile_definitions(${target} PRIVATE WALLY_HAS_ONNX=1)
    endif()
    if(RunAnywhere_HAS_SHERPA)
        target_compile_definitions(${target} PRIVATE WALLY_HAS_SHERPA=1)
    endif()
    if(RunAnywhere_HAS_MLX)
        target_compile_definitions(${target} PRIVATE WALLY_HAS_MLX=1)
    endif()
    if(RunAnywhere_HAS_CLOUD)
        target_compile_definitions(${target} PRIVATE WALLY_HAS_CLOUD=1)
    endif()
    if(TARGET RunAnywhere::neurt)
        target_compile_definitions(${target} PRIVATE WALLY_HAS_NEURT=1)
        target_link_libraries(${target} PRIVATE RunAnywhere::neurt)
    endif()
    if(TARGET RunAnywhere::qhexrt)
        target_compile_definitions(${target} PRIVATE WALLY_HAS_QHEXRT=1)
        target_link_libraries(${target} PRIVATE RunAnywhere::qhexrt)
    endif()
    if(RunAnywhere_HAS_RAG)
        target_compile_definitions(${target} PRIVATE WALLY_HAS_RAG=1)
    endif()
    if(RunAnywhere_HAS_SERVER)
        target_compile_definitions(${target} PRIVATE WALLY_HAS_SERVER=1)
    endif()
    if(TARGET RunAnywhere::server)
        target_link_libraries(${target} PRIVATE RunAnywhere::server)
        # The kit's librac_server.a calls into libcrypto (OPENSSL_thread_stop,
        # from httplib's thread pool) but its imported target does not say so,
        # so nothing here asks for OpenSSL and the link only succeeds on a
        # machine where CMake happened to find it anyway. On a clean consumer
        # it configures fine and then fails at the very end with an undefined
        # symbol (wally #92). Ask for it here, and say so at configure time.
        # Windows adds no find_package/link here: the linker resolves that
        # OPENSSL_thread_stop against whatever OpenSSL sits on the build machine,
        # so the link succeeds without us asking. That is not free -- it leaves
        # wally.exe importing libssl-3/libcrypto-3, which must ship beside it or a
        # clean machine fails at launch with 0xC0000135 (wally #122). Those two
        # DLLs are bundled in wally_bundle_product_dlls below, so the
        # Windows archive is self-contained without a link step here.
        if(NOT WIN32)
            # Homebrew's openssl@3 is keg-only, so it is not on the default
            # search path and a bare find_package misses it on an otherwise
            # perfectly equipped Mac. Ask brew where it is before giving up:
            # failing with "install openssl" on a machine that already has it
            # is a worse outcome than the link error this replaces.
            if(APPLE AND NOT DEFINED OPENSSL_ROOT_DIR AND NOT DEFINED ENV{OPENSSL_ROOT_DIR})
                find_program(WALLY_BREW brew)
                if(WALLY_BREW)
                    execute_process(COMMAND "${WALLY_BREW}" --prefix openssl@3
                                    OUTPUT_VARIABLE _brew_openssl
                                    OUTPUT_STRIP_TRAILING_WHITESPACE
                                    ERROR_QUIET)
                    if(_brew_openssl AND EXISTS "${_brew_openssl}")
                        set(OPENSSL_ROOT_DIR "${_brew_openssl}")
                    endif()
                endif()
            endif()
            find_package(OpenSSL QUIET)
            if(NOT OpenSSL_FOUND)
                message(FATAL_ERROR
                    "The RunAnywhere kit's server component needs OpenSSL, and it was not found.\n"
                    "  macOS:  brew install openssl@3, then configure with "
                    "-DOPENSSL_ROOT_DIR=$(brew --prefix openssl@3)\n"
                    "  Linux:  install libssl-dev (or openssl-devel)\n"
                    "Configure with -DWALLY_SDK_KIT pointing at a kit without the server "
                    "component if you do not need `wally serve`.")
            endif()
            target_link_libraries(${target} PRIVATE OpenSSL::Crypto OpenSSL::SSL)
            if(NOT WALLY_REPORTED_OPENSSL)
                message(STATUS "wally: OpenSSL ${OPENSSL_VERSION} for the kit's server component")
                set(WALLY_REPORTED_OPENSSL ON CACHE INTERNAL "")
            endif()
        endif()
    endif()
endfunction()

# Win32 LoadLibrary searches the exe directory then PATH. Kit third_party
# (onnxruntime.dll, sherpa-onnx-c-api.dll, …) must sit next to every binary
# that links wally_core — wally.exe and the unit-test exes. Linking the import
# lib is not enough; 0xc0000135 is a missing DLL at process start.
# `dest_dir` is where the executable lands; `target` is the (custom) target
# whose POST_BUILD copies the DLLs there.
function(wally_stage_windows_runtime_dlls target dest_dir)
    if(NOT WIN32)
        return()
    endif()
    if(DEFINED RunAnywhere_THIRD_PARTY_DIR AND EXISTS "${RunAnywhere_THIRD_PARTY_DIR}")
        file(GLOB _wally_tp_dlls "${RunAnywhere_THIRD_PARTY_DIR}/*.dll")
        foreach(_dll IN LISTS _wally_tp_dlls)
            get_filename_component(_dll_name "${_dll}" NAME)
            add_custom_command(TARGET ${target} POST_BUILD
                COMMAND ${CMAKE_COMMAND} -E copy_if_different
                    "${_dll}"
                    "${dest_dir}/${_dll_name}"
                COMMENT "Stage ${_dll_name} into ${dest_dir}"
                VERBATIM)
        endforeach()
    endif()
    if(DEFINED RunAnywhere_LIBRARY_DIR)
        add_custom_command(TARGET ${target} POST_BUILD
            COMMAND ${CMAKE_COMMAND}
                "-DSRC_DIR=${RunAnywhere_LIBRARY_DIR}/../bin"
                "-DDST_DIR=${dest_dir}"
                -P "${CMAKE_SOURCE_DIR}/cmake/copy-overlay-dlls.cmake"
            COMMENT "Stage overlay DLLs into ${dest_dir}"
            VERBATIM)
    endif()
endfunction()

# The MSVC runtime and OpenSSL DLLs only have to ship with the product exe, not
# beside every test binary: the tests run under the build environment's PATH,
# where those DLLs already resolve. Staging them for all ~12 test targets made a
# dozen POST_BUILD commands copy one DLL into build/tests/ at once, which Windows
# fails with a sharing violation (wally #122). Bundle them for the product only.
function(wally_bundle_product_dlls target dest_dir)
    if(NOT WIN32)
        return()
    endif()
    # The exe links the MSVC runtime dynamically (vcruntime140.dll,
    # vcruntime140_1.dll on arm64, msvcp140.dll). Those live in the toolchain, so
    # a build machine resolves them on PATH but a clean user machine without the
    # VC++ redistributable does not -- 0xC0000135 at launch. Stage them so the
    # archive carries its own runtime.
    if(MSVC)
        set(CMAKE_INSTALL_SYSTEM_RUNTIME_LIBS_SKIP TRUE)
        include(InstallRequiredSystemLibraries)
        if(NOT CMAKE_INSTALL_SYSTEM_RUNTIME_LIBS)
            message(WARNING
                "wally: InstallRequiredSystemLibraries found no MSVC runtime to "
                "bundle; the Windows archive may fail with 0xC0000135 on a clean "
                "machine. Check the toolset and arch of the configure environment.")
        endif()
        foreach(_rt IN LISTS CMAKE_INSTALL_SYSTEM_RUNTIME_LIBS)
            get_filename_component(_rt_name "${_rt}" NAME)
            add_custom_command(TARGET ${target} POST_BUILD
                COMMAND ${CMAKE_COMMAND} -E copy_if_different
                    "${_rt}"
                    "${dest_dir}/${_rt_name}"
                COMMENT "Stage ${_rt_name} into ${dest_dir}"
                VERBATIM)
        endforeach()
    endif()
    # rac_server.lib imports one OpenSSL symbol (OPENSSL_thread_stop), which the
    # linker resolves against the build machine's OpenSSL, so the exe ends up
    # importing libssl-3/libcrypto-3. httplib is built without TLS here, so this
    # is a link-time artefact, not real crypto -- but the DLLs still have to ship
    # beside the exe or the archive fails with 0xC0000135 on a machine without
    # OpenSSL. Bundle the two the exe actually imports.
    if(TARGET RunAnywhere::server)
        if(CMAKE_SYSTEM_PROCESSOR MATCHES "ARM64|arm64|aarch64")
            set(_wally_ssl_arch "arm64")
        else()
            set(_wally_ssl_arch "x64")
        endif()
        foreach(_ossl libssl libcrypto)
            # Only the arch-suffixed name can satisfy a 64-bit exe's import table;
            # the un-suffixed libssl-3.dll is OpenSSL's 32-bit x86 spelling, so a
            # stray one on PATH would stage a wrong-arch DLL. NO_CACHE re-resolves
            # every configure, so reusing a build tree across arches cannot pin a
            # stale path.
            find_file(WALLY_${_ossl}_DLL
                NAMES "${_ossl}-3-${_wally_ssl_arch}.dll"
                PATHS ENV PATH
                PATH_SUFFIXES bin
                NO_CACHE)
            if(WALLY_${_ossl}_DLL)
                get_filename_component(_ossl_name "${WALLY_${_ossl}_DLL}" NAME)
                add_custom_command(TARGET ${target} POST_BUILD
                    COMMAND ${CMAKE_COMMAND} -E copy_if_different
                        "${WALLY_${_ossl}_DLL}"
                        "${dest_dir}/${_ossl_name}"
                    COMMENT "Stage ${_ossl_name} into ${dest_dir}"
                    VERBATIM)
            else()
                message(WARNING
                    "wally: ${_ossl}-3-${_wally_ssl_arch}.dll not found on PATH to "
                    "bundle; the Windows archive may fail with 0xC0000135 on a "
                    "clean machine.")
            endif()
        endforeach()
    endif()
endfunction()
