#!/usr/bin/env bash
# Kit-consumer e2e for a built wally binary. Does not compile the SDK.
#
#   scripts/test/e2e.sh <path-to-wally>
#
# Always runs scripts/test/smoke.sh (no model download). Set WALLY_E2E_MODEL to also
# pull a catalog model and run one generation — that needs network + disk.
# Overlay / device round-trips live in scripts/test/e2e-modalities.sh and are keyed
# by primitive, not engine. Public CI leaves those knobs unset (skip).
# CMAKE_PREFIX_PATH is optional; WALLY_SDK_KIT is preferred. Unset is fine.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
WALLY="${1:?usage: e2e.sh <path-to-wally>}"
if [[ ! -e "${WALLY}" ]]; then
    echo "not found: ${WALLY}" >&2
    exit 1
fi
if [[ ! -x "${WALLY}" && "${WALLY}" != *.exe && "${WALLY}" != *.EXE ]]; then
    echo "not executable: ${WALLY}" >&2
    exit 1
fi

kit_cfg=""
kit_root="${WALLY_SDK_KIT:-${CMAKE_PREFIX_PATH-}}"
# CMAKE_PREFIX_PATH may be a list: ':' separated on POSIX, ';' on Windows. Take
# the first entry, but never split a Windows path at its drive-letter colon --
# doing that turned "D:/a/WALLY/kit" into "D", the kit Config was then never
# found, and the backend check silently fell back to expecting llamacpp + onnx
# + sherpa. On x64 that default happens to be correct so the bug stayed hidden;
# on ARM64, whose kit ships none of them, it failed the job.
case "${kit_root}" in
    *\;*) kit_root="${kit_root%%;*}" ;;
esac
case "${kit_root}" in
    ?:[/\\]*) ;;                       # C:/... or C:\... — leave the drive alone
    *) kit_root="${kit_root%%:*}" ;;
esac
if [[ -n "${kit_root}" ]] && command -v cygpath >/dev/null 2>&1; then
    kit_root="$(cygpath -u "${kit_root}")"
fi
if [[ -n "${kit_root}" ]]; then
    for cand in \
        "${kit_root}/lib/cmake/RunAnywhere/RunAnywhereConfig.cmake" \
        "${kit_root}/lib/cmake/runanywhere/RunAnywhereConfig.cmake"; do
        if [[ -f "${cand}" ]]; then
            kit_cfg="${cand}"
            break
        fi
    done
fi

# Win32 LoadLibrary searches the exe directory, then PATH. Stage kit DLLs
# next to wally.exe and put third_party on PATH so onnx/sherpa can register
# even if CMake's configure-time GLOB missed a file.
if [[ "${WALLY}" == *.exe || "${WALLY}" == *.EXE ]] && [[ -n "${kit_root}" ]]; then
    exe_dir="$(cd "$(dirname "${WALLY}")" && pwd)"
    for d in "${kit_root}/third_party" "${kit_root}/bin" "${kit_root}/lib"; do
        [[ -d "${d}" ]] || continue
        if command -v cygpath >/dev/null 2>&1; then
            PATH="$(cygpath -u "${d}"):${PATH}"
        else
            PATH="${d}:${PATH}"
        fi
        find "${d}" -maxdepth 1 \( -iname '*.dll' \) -exec cp -n {} "${exe_dir}/" \; 2>/dev/null || true
    done
    export PATH
fi

bash "${ROOT}/scripts/test/smoke.sh" "${WALLY}"

fail=0
check() {
    local label="$1"; shift
    if "$@" >/dev/null 2>&1; then
        echo "  ok    ${label}"
    else
        echo "  FAIL  ${label}"
        fail=1
    fi
}

echo "e2e (modelless extras): ${WALLY}"
check "info" "${WALLY}" info

# Assert the engines this binary's kit actually ships. A 0.20.26 Windows kit
# may only advertise llamacpp; 0.20.28+ kits advertise onnx + sherpa too.
# Product `wally` on Apple Silicon also includes MLX. Intermediate `wally-cxx` does not.

expected_backends=()
kit_flag_true() {
    local key="$1"
    [[ -n "${kit_cfg}" ]] || return 0
    grep -Eq "set\\(${key}[[:space:]]+(TRUE|ON|1)\\)" "${kit_cfg}"
}
# When the kit Config is present, honor HAS_* (Windows ARM64 public kits have
# no llama.cpp/ONNX/Sherpa). Without a kit, assume the public OSS bottle.
if [[ -z "${kit_cfg}" ]]; then
    expected_backends=(llamacpp onnx sherpa)
else
    if kit_flag_true RunAnywhere_HAS_LLAMACPP; then
        expected_backends+=(llamacpp)
    fi
    if kit_flag_true RunAnywhere_HAS_ONNX; then
        expected_backends+=(onnx)
    fi
    if kit_flag_true RunAnywhere_HAS_SHERPA; then
        expected_backends+=(sherpa)
    fi
fi
base="$(basename "${WALLY}")"
base="${base%.exe}"
if [[ "$(uname -s)" == Darwin && "$(uname -m)" == arm64 && "${base}" == "wally" ]]; then
    expected_backends+=(mlx)
fi
# Overlay archives flip HAS_NEURT/HAS_QHEXRT at find_package time, but the
# packaged Config.cmake still says FALSE. Presence of the backend library is
# the source of truth. Only WALLY_SDK_KIT counts — ambient CMAKE_PREFIX_PATH
# from an overlay rebuild must not fail a public-bottle e2e.
overlay_root="${WALLY_SDK_KIT-}"
if [[ -n "${overlay_root}" ]] && command -v cygpath >/dev/null 2>&1; then
    overlay_root="$(cygpath -u "${overlay_root}")"
fi
if [[ -n "${overlay_root}" ]]; then
    if [[ -f "${overlay_root}/lib/librac_backend_neurt.a" || -f "${overlay_root}/lib/rac_backend_neurt.lib" ]]; then
        expected_backends+=(neurt)
    fi
    if [[ -f "${overlay_root}/lib/librac_backend_qhexrt.a" || -f "${overlay_root}/lib/rac_backend_qhexrt.lib" ]]; then
        expected_backends+=(qhexrt)
    fi
fi
if [[ ${#expected_backends[@]} -eq 0 ]]; then
    echo "  skip  backends (kit has no engines)"
elif bash "${ROOT}/scripts/test/assert-backends.sh" "${WALLY}" "${expected_backends[@]}"; then
    echo "  ok    backends ${expected_backends[*]}"
else
    echo "  FAIL  backends ${expected_backends[*]}"
    fail=1
fi
if [[ ${#expected_backends[@]} -gt 0 ]]; then
    if bash "${ROOT}/scripts/test/assert-binary-backends.sh" "${WALLY}" "${expected_backends[@]}"; then
        echo "  ok    binary ${expected_backends[*]}"
    else
        echo "  FAIL  binary ${expected_backends[*]}"
        fail=1
    fi
fi
if "${WALLY}" models list --help >/dev/null 2>&1; then
    check "models list" "${WALLY}" models list
fi

# Per-backend help surfaces that only compile in when the engine is linked.
for name in "${expected_backends[@]}"; do
    case "${name}" in
        neurt)
            check "image generate --help" "${WALLY}" image generate --help
            ;;
        mlx|qhexrt)
            check "run --help" "${WALLY}" run --help
            ;;
    esac
done

# Modality round-trips (engine-agnostic). Skip when no model is discovered.
# WALLY_E2E_MODALITIES=0 disables this so public CI can stay modelless-only.
if [[ "${WALLY_E2E_MODALITIES:-1}" != "0" ]]; then
    if bash "${ROOT}/scripts/test/e2e-modalities.sh" "${WALLY}"; then
        echo "  ok    modalities"
    else
        echo "  FAIL  modalities"
        fail=1
    fi
else
    echo "  skip  modalities (WALLY_E2E_MODALITIES=0)"
fi

# The OpenWork desktop app on wally's private profile; opt-in, macOS only.
if bash "${ROOT}/scripts/test/e2e-openwork.sh" "${WALLY}"; then
    :
else
    echo "  FAIL  openwork"
    fail=1
fi

exit "${fail}"
