# Exercise production CLI parsing, launch and the packaged SDK HTTP server;
# only model inference and the external coding tool are deterministic fixtures.
if(RunAnywhere_HAS_SERVER AND RunAnywhere_HAS_LLAMACPP)
    add_executable(wally_fake_llm fake_llm.cpp)
    target_link_libraries(wally_fake_llm PRIVATE wally_core nlohmann_json::nlohmann_json)
    wally_stage_windows_runtime_dlls(wally_fake_llm)

    add_executable(wally_harness_child harness_child.cpp)
    target_link_libraries(wally_harness_child PRIVATE httplib::httplib nlohmann_json::nlohmann_json)
    target_compile_features(wally_harness_child PRIVATE cxx_std_20)
    add_test(NAME wally_local_harness_cli_e2e
        COMMAND "${Python3_EXECUTABLE}" "${CMAKE_CURRENT_SOURCE_DIR}/test_local_harness.py"
                "$<TARGET_FILE:wally_fake_llm>" "$<TARGET_FILE:wally_harness_child>")
    set_tests_properties(wally_local_harness_cli_e2e PROPERTIES TIMEOUT 120)
endif()
