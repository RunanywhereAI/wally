# CLI parse/REPL/output tests. Proto headers come from the kit via wally_core.
enable_testing()

add_executable(test_wally_unit test_wally_unit.cpp)
target_include_directories(test_wally_unit PRIVATE "${CMAKE_CURRENT_SOURCE_DIR}")
target_link_libraries(test_wally_unit PRIVATE wally_core nlohmann_json::nlohmann_json)
wally_stage_windows_runtime_dlls(test_wally_unit)
add_test(NAME wally_unit_tests COMMAND test_wally_unit --run-all)

add_executable(test_wally_account test_wally_account.cpp)
target_include_directories(test_wally_account PRIVATE "${CMAKE_CURRENT_SOURCE_DIR}")
# httplib: the timeout test stands up a server that never answers (#81).
target_link_libraries(test_wally_account PRIVATE wally_core httplib::httplib nlohmann_json::nlohmann_json)
wally_stage_windows_runtime_dlls(test_wally_account)
add_test(NAME wally_account_tests COMMAND test_wally_account --run-all)

add_executable(test_wally_contract test_wally_contract.cpp)
target_include_directories(test_wally_contract PRIVATE "${CMAKE_CURRENT_SOURCE_DIR}")
target_compile_definitions(test_wally_contract PRIVATE
    "WALLY_CONTRACT_PATH=\"${CMAKE_CURRENT_SOURCE_DIR}/../contracts/wally-cli-v1.openapi.json\"")
target_link_libraries(test_wally_contract PRIVATE wally_core nlohmann_json::nlohmann_json)
wally_stage_windows_runtime_dlls(test_wally_contract)
add_test(NAME wally_contract_tests COMMAND test_wally_contract --run-all)

add_executable(test_wally_opencode test_wally_opencode.cpp)
target_include_directories(test_wally_opencode PRIVATE "${CMAKE_CURRENT_SOURCE_DIR}")
target_link_libraries(test_wally_opencode PRIVATE wally_core nlohmann_json::nlohmann_json)
wally_stage_windows_runtime_dlls(test_wally_opencode)
add_test(NAME wally_opencode_tests COMMAND test_wally_opencode --run-all)

add_executable(test_wally_codex test_wally_codex.cpp)
target_include_directories(test_wally_codex PRIVATE "${CMAKE_CURRENT_SOURCE_DIR}")
target_link_libraries(test_wally_codex PRIVATE wally_core nlohmann_json::nlohmann_json)
wally_stage_windows_runtime_dlls(test_wally_codex)
add_test(NAME wally_codex_tests COMMAND test_wally_codex --run-all)

add_executable(test_wally_anthropic test_wally_anthropic.cpp)
target_include_directories(test_wally_anthropic PRIVATE "${CMAKE_CURRENT_SOURCE_DIR}")
# httplib is private to wally_core; this suite runs a fake upstream server of
# its own, so it links the header directly.
target_link_libraries(test_wally_anthropic PRIVATE wally_core httplib::httplib nlohmann_json::nlohmann_json)
wally_stage_windows_runtime_dlls(test_wally_anthropic)
add_test(NAME wally_anthropic_tests COMMAND test_wally_anthropic --run-all)
set_tests_properties(wally_anthropic_tests PROPERTIES TIMEOUT 60)

add_executable(test_wally_net_call test_wally_net_call.cpp)
target_include_directories(test_wally_net_call PRIVATE "${CMAKE_CURRENT_SOURCE_DIR}")
target_link_libraries(test_wally_net_call PRIVATE wally_core httplib::httplib nlohmann_json::nlohmann_json)
wally_stage_windows_runtime_dlls(test_wally_net_call)
add_test(NAME wally_net_call_tests COMMAND test_wally_net_call --run-all)
set_tests_properties(wally_net_call_tests PROPERTIES TIMEOUT 60)

add_executable(test_wally_ide_proxy test_wally_ide_proxy.cpp)
target_include_directories(test_wally_ide_proxy PRIVATE "${CMAKE_CURRENT_SOURCE_DIR}")
target_link_libraries(test_wally_ide_proxy PRIVATE wally_core httplib::httplib nlohmann_json::nlohmann_json)
wally_stage_windows_runtime_dlls(test_wally_ide_proxy)
add_test(NAME wally_ide_proxy_tests COMMAND test_wally_ide_proxy --run-all)
set_tests_properties(wally_ide_proxy_tests PROPERTIES TIMEOUT 60)

add_executable(test_wally_harness test_wally_harness.cpp)
target_include_directories(test_wally_harness PRIVATE "${CMAKE_CURRENT_SOURCE_DIR}")
target_link_libraries(test_wally_harness PRIVATE wally_core nlohmann_json::nlohmann_json)
wally_stage_windows_runtime_dlls(test_wally_harness)
add_test(NAME wally_harness_tests COMMAND test_wally_harness --run-all)

find_package(Python3 REQUIRED COMPONENTS Interpreter)
add_test(
    NAME wally_account_cli_e2e
    COMMAND "${Python3_EXECUTABLE}" "${CMAKE_CURRENT_SOURCE_DIR}/test_account_cli.py"
            "$<TARGET_FILE:wally>")
# 20s was too tight and failed on the macOS runner: this launches the wally
# binary three times (login, whoami, logout) and each launch initialises the
# whole SDK — the MLX e2e test alone takes ~10s on the same machine. The budget
# is per-test, not per-command, and the individual commands are already bounded
# by the 15s subprocess timeout inside test_account_cli.py, so a real hang is
# still caught there rather than here.
set_tests_properties(wally_account_cli_e2e PROPERTIES TIMEOUT 120)

add_executable(test_wally_segment test_wally_segment.cpp)
target_include_directories(test_wally_segment PRIVATE "${CMAKE_CURRENT_SOURCE_DIR}")
target_link_libraries(test_wally_segment PRIVATE wally_core)
wally_stage_windows_runtime_dlls(test_wally_segment)
add_test(NAME wally_segment_tests COMMAND test_wally_segment --run-all)

add_executable(test_wally_telemetry_live test_wally_telemetry_live.cpp)
target_include_directories(test_wally_telemetry_live PRIVATE "${CMAKE_CURRENT_SOURCE_DIR}")
target_link_libraries(test_wally_telemetry_live PRIVATE wally_core)
wally_stage_windows_runtime_dlls(test_wally_telemetry_live)
add_test(NAME wally_telemetry_live_tests COMMAND test_wally_telemetry_live)

if(APPLE AND RunAnywhere_HAS_MLX)
    add_executable(test_wally_mlx_e2e test_wally_mlx_e2e.cpp)
    target_include_directories(test_wally_mlx_e2e PRIVATE "${CMAKE_CURRENT_SOURCE_DIR}")
    target_link_libraries(test_wally_mlx_e2e PRIVATE wally_core)
    wally_stage_windows_runtime_dlls(test_wally_mlx_e2e)
    add_test(NAME wally_mlx_e2e_tests COMMAND test_wally_mlx_e2e --run-all)
endif()
