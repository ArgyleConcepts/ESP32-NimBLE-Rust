if(ARGYLE_SCENARIO STREQUAL "early_directory_rule_launch")
    set_property(DIRECTORY PROPERTY RULE_LAUNCH_COMPILE "${CMAKE_COMMAND} -E env")
endif()

idf_component_register(SRCS "consumer.c" INCLUDE_DIRS ".")
set_property(GLOBAL PROPERTY ARGYLE_FIXTURE_CONSUMER_TARGET "${COMPONENT_LIB}")

if(ARGYLE_SCENARIO STREQUAL "early_target_rule_launch")
    set_property(TARGET "${COMPONENT_LIB}" PROPERTY RULE_LAUNCH_COMPILE
        "${CMAKE_COMMAND} -E env")
elseif(ARGYLE_SCENARIO STREQUAL "early_consumer_c_compiler_launcher")
    set_property(TARGET "${COMPONENT_LIB}" PROPERTY C_COMPILER_LAUNCHER
        "${CMAKE_COMMAND};-E;env")
endif()

if(ARGYLE_SCENARIO STREQUAL "positive_usage_and_late_properties")
    # This directory option is added after idf_component_register created the
    # consumer and immediately before the exporter creates its probe.
    add_compile_options(-DARGYLE_DIRECTORY_LEAK=1)

    add_library(argyle_fixture_transitive INTERFACE)
    target_include_directories(argyle_fixture_transitive INTERFACE
        "$<$<CONFIG:Release>:${CMAKE_CURRENT_LIST_DIR}/transitive include>")
    target_compile_options(argyle_fixture_transitive INTERFACE
        "$<$<CONFIG:Release>:-DARGYLE_TRANSITIVE_OPTION=1>")
    target_link_libraries("${COMPONENT_LIB}" PRIVATE argyle_fixture_transitive)
endif()

if(NOT ARGYLE_SCENARIO MATCHES "^guard_")
    argyle_nimble_export_build_context(
        CONSUMER_TARGET "${COMPONENT_LIB}"
        OUTPUT "${CMAKE_BINARY_DIR}/argyle-export-context.json")
    file(WRITE "${CMAKE_BINARY_DIR}/argyle-consumer-target.txt" "${COMPONENT_LIB}\n")

    # This dependency direction models a consumer that needs the export before
    # its own compilation. If the probe gains a link edge back to the consumer,
    # CMake's generated dependency graph contains a cycle.
    add_dependencies("${COMPONENT_LIB}" "${ARGYLE_NIMBLE_BUILD_CONTEXT_TARGET}")
endif()

if(ARGYLE_SCENARIO STREQUAL "positive_usage_and_late_properties")
    cmake_language(DEFER DIRECTORY "${CMAKE_SOURCE_DIR}"
        CALL argyle_fixture_assert_scalar_copy)
endif()
