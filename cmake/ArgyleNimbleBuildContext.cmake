include_guard(GLOBAL)

function(_argyle_nimble_reject_compile_launchers consumer probe)
    set(_argyle_has_custom_compile_launcher FALSE)

    get_property(_argyle_global_rule_launch_set GLOBAL PROPERTY RULE_LAUNCH_COMPILE SET)
    if(_argyle_global_rule_launch_set)
        get_property(_argyle_global_rule_launch GLOBAL PROPERTY RULE_LAUNCH_COMPILE)
        if(NOT "${_argyle_global_rule_launch}" STREQUAL "")
            set(_argyle_has_custom_compile_launcher TRUE)
        endif()
    endif()

    set(_argyle_targets "${consumer}")
    if(probe)
        list(APPEND _argyle_targets "${probe}")
    endif()
    foreach(_argyle_target IN LISTS _argyle_targets)
        get_target_property(_argyle_target_binary_dir "${_argyle_target}" BINARY_DIR)
        set(_argyle_directory "${_argyle_target_binary_dir}")
        while(_argyle_directory)
            get_property(_argyle_directory_rule_launch_set DIRECTORY "${_argyle_directory}"
                PROPERTY RULE_LAUNCH_COMPILE SET)
            if(_argyle_directory_rule_launch_set)
                get_property(_argyle_directory_rule_launch DIRECTORY "${_argyle_directory}"
                    PROPERTY RULE_LAUNCH_COMPILE)
                if(NOT "${_argyle_directory_rule_launch}" STREQUAL "")
                    set(_argyle_has_custom_compile_launcher TRUE)
                endif()
            endif()
            get_property(_argyle_parent_source_dir DIRECTORY "${_argyle_directory}"
                PROPERTY PARENT_DIRECTORY)
            if(_argyle_parent_source_dir)
                get_property(_argyle_parent_binary_dir DIRECTORY "${_argyle_parent_source_dir}"
                    PROPERTY BINARY_DIR)
                if(NOT _argyle_parent_binary_dir)
                    message(FATAL_ERROR
                        "Could not identify the parent CMake binary directory while checking compile launchers")
                endif()
                set(_argyle_directory "${_argyle_parent_binary_dir}")
            else()
                set(_argyle_directory "")
            endif()
        endwhile()

        get_property(_argyle_target_rule_launch_set TARGET "${_argyle_target}"
            PROPERTY RULE_LAUNCH_COMPILE SET)
        if(_argyle_target_rule_launch_set)
            get_target_property(_argyle_target_rule_launch "${_argyle_target}"
                RULE_LAUNCH_COMPILE)
            if(NOT "${_argyle_target_rule_launch}" STREQUAL "")
                set(_argyle_has_custom_compile_launcher TRUE)
            endif()
        endif()
    endforeach()

    get_property(_argyle_consumer_compiler_launcher_set TARGET "${consumer}"
        PROPERTY C_COMPILER_LAUNCHER SET)
    if(_argyle_consumer_compiler_launcher_set)
        get_target_property(_argyle_consumer_compiler_launcher "${consumer}"
            C_COMPILER_LAUNCHER)
        if(NOT "${_argyle_consumer_compiler_launcher}" STREQUAL "")
            set(_argyle_has_custom_compile_launcher TRUE)
        endif()
    endif()

    if(_argyle_has_custom_compile_launcher)
        message(FATAL_ERROR
            "Build-context export cannot preserve custom CMake compile launchers (including ccache); disable RULE_LAUNCH_COMPILE at global scope, in each target directory and every ancestor directory, and on target scope, and clear the consumer target's C_COMPILER_LAUNCHER")
    endif()
endfunction()

function(_argyle_nimble_copy_scalar_compile_properties consumer probe)
    get_target_property(_argyle_consumer_type "${consumer}" TYPE)
    get_target_property(_argyle_consumer_binary_dir "${consumer}" BINARY_DIR)
    get_directory_property(_argyle_consumer_build_type DIRECTORY "${_argyle_consumer_binary_dir}"
        DEFINITION CMAKE_BUILD_TYPE)
    foreach(_argyle_property IN ITEMS C_STANDARD C_STANDARD_REQUIRED C_EXTENSIONS
        POSITION_INDEPENDENT_CODE C_VISIBILITY_PRESET INTERPROCEDURAL_OPTIMIZATION)
        get_property(_argyle_property_set TARGET "${consumer}" PROPERTY "${_argyle_property}" SET)
        if(_argyle_property_set)
            get_target_property(_argyle_value "${consumer}" "${_argyle_property}")
            set_property(TARGET "${probe}" PROPERTY "${_argyle_property}" "${_argyle_value}")
        elseif(_argyle_property STREQUAL "POSITION_INDEPENDENT_CODE"
            AND (_argyle_consumer_type STREQUAL "SHARED_LIBRARY"
                OR _argyle_consumer_type STREQUAL "MODULE_LIBRARY"))
            set_property(TARGET "${probe}" PROPERTY POSITION_INDEPENDENT_CODE TRUE)
        else()
            set_property(TARGET "${probe}" PROPERTY "${_argyle_property}")
        endif()
    endforeach()

    # IPO has a configuration-specific override. The contract rejects
    # multi-config generators, so copy the selected single-config override
    # when one exists as well as the generic property above.
    if(_argyle_consumer_build_type)
        string(TOUPPER "${_argyle_consumer_build_type}" _argyle_build_type_upper)
        set(_argyle_ipo_config_property
            "INTERPROCEDURAL_OPTIMIZATION_${_argyle_build_type_upper}")
        get_property(_argyle_ipo_config_set TARGET "${consumer}"
            PROPERTY "${_argyle_ipo_config_property}" SET)
        if(_argyle_ipo_config_set)
            get_target_property(_argyle_ipo_config_value "${consumer}"
                "${_argyle_ipo_config_property}")
            set_property(TARGET "${probe}" PROPERTY "${_argyle_ipo_config_property}"
                "${_argyle_ipo_config_value}")
        else()
            set_property(TARGET "${probe}" PROPERTY "${_argyle_ipo_config_property}")
        endif()
    endif()
endfunction()

function(_argyle_nimble_finalize_build_context_export)
    get_property(_argyle_consumer_target GLOBAL PROPERTY ARGYLE_NIMBLE_CONTEXT_CONSUMER_TARGET)
    get_property(_argyle_probe_target GLOBAL PROPERTY ARGYLE_NIMBLE_CONTEXT_PROBE_TARGET)
    get_property(_argyle_expected_probe_launcher GLOBAL PROPERTY ARGYLE_NIMBLE_CONTEXT_PROBE_LAUNCHER)

    if(NOT _argyle_consumer_target OR NOT _argyle_probe_target)
        message(FATAL_ERROR
            "Deferred build-context validation has incomplete target state; re-run the configured ESP-IDF CMake project")
    endif()
    _argyle_nimble_reject_compile_launchers(
        "${_argyle_consumer_target}" "${_argyle_probe_target}")

    get_property(_argyle_probe_launcher_set TARGET "${_argyle_probe_target}"
        PROPERTY C_COMPILER_LAUNCHER SET)
    if(_argyle_probe_launcher_set)
        get_target_property(_argyle_probe_launcher "${_argyle_probe_target}" C_COMPILER_LAUNCHER)
        if(NOT "${_argyle_probe_launcher}" STREQUAL "${_argyle_expected_probe_launcher}")
            message(FATAL_ERROR
                "Build-context probe compiler launcher changed during CMake configuration; preserve the configured argv capture launcher")
        endif()
    else()
        message(FATAL_ERROR
            "Build-context probe compiler launcher was removed during CMake configuration; preserve the configured argv capture launcher")
    endif()

    _argyle_nimble_copy_scalar_compile_properties("${_argyle_consumer_target}" "${_argyle_probe_target}")
endfunction()

# Export the actual configured consumer C context through a one-source probe.
# The compile launcher records argv as process arguments; no command text is
# split or reinterpreted by a shell.
function(argyle_nimble_export_build_context)
    cmake_parse_arguments(PARSE_ARGV 0 ARG "" "CONSUMER_TARGET;OUTPUT" "")

    if(NOT ARG_CONSUMER_TARGET OR NOT TARGET "${ARG_CONSUMER_TARGET}")
        message(FATAL_ERROR
            "argyle_nimble_export_build_context requires CONSUMER_TARGET to name an existing configured CMake target")
    endif()
    get_target_property(_argyle_consumer_source_dir "${ARG_CONSUMER_TARGET}" SOURCE_DIR)
    get_target_property(_argyle_consumer_binary_dir "${ARG_CONSUMER_TARGET}" BINARY_DIR)
    if(NOT "${_argyle_consumer_source_dir}" STREQUAL "${CMAKE_CURRENT_SOURCE_DIR}"
        OR NOT "${_argyle_consumer_binary_dir}" STREQUAL "${CMAKE_CURRENT_BINARY_DIR}")
        message(FATAL_ERROR
            "argyle_nimble_export_build_context must be called from the consumer target's defining CMake source and binary directory so directory-scoped compiler flags match")
    endif()
    if(NOT ESP_PLATFORM OR NOT COMMAND idf_build_get_property)
        message(FATAL_ERROR
            "argyle_nimble_export_build_context must run inside a configured ESP-IDF CMake project")
    endif()

    # ESP-IDF 6.1 stores these values as build properties. Query them from the
    # active consumer configuration instead of relying on process-global SDK
    # environment values or similarly named cache entries.
    idf_build_get_property(_argyle_idf_path IDF_PATH)
    idf_build_get_property(_argyle_idf_target IDF_TARGET)
    idf_build_get_property(_argyle_idf_target_arch IDF_TARGET_ARCH)
    idf_build_get_property(_argyle_idf_version IDF_VER)
    idf_build_get_property(_argyle_sdkconfig SDKCONFIG)
    idf_build_get_property(_argyle_sdkconfig_header SDKCONFIG_HEADER)
    idf_build_get_property(_argyle_build_dir BUILD_DIR)
    if(NOT _argyle_idf_path OR NOT _argyle_idf_target OR NOT _argyle_idf_target_arch
        OR NOT _argyle_idf_version OR NOT _argyle_sdkconfig OR NOT _argyle_sdkconfig_header
        OR NOT _argyle_build_dir)
        message(FATAL_ERROR
            "ESP-IDF build properties are incomplete; run this exporter after the consumer project is configured")
    endif()
    if(NOT CMAKE_C_COMPILER)
        message(FATAL_ERROR
            "The selected ESP-IDF C compiler is unavailable; finish CMake configuration before exporting context")
    endif()
    if(CMAKE_CONFIGURATION_TYPES)
        message(FATAL_ERROR
            "Build-context export requires a single-config CMake generator; configure Ninja or Makefiles so captured flags identify one configuration")
    endif()

    if(NOT (_argyle_idf_target STREQUAL "esp32c3" AND _argyle_idf_target_arch STREQUAL "riscv")
        AND NOT (_argyle_idf_target STREQUAL "esp32s3" AND _argyle_idf_target_arch STREQUAL "xtensa"))
        message(FATAL_ERROR
            "Unsupported ESP-IDF target/architecture: this crate supports ESP32-C3/riscv and ESP32-S3/xtensa")
    endif()

    # A compile launcher may rewrite or skip the probe invocation. Preserve
    # the consumer's selected compiler argv by requiring an unwrapped probe.
    _argyle_nimble_reject_compile_launchers(
        "${ARG_CONSUMER_TARGET}" "")

    find_package(Python3 REQUIRED COMPONENTS Interpreter)
    find_package(Git REQUIRED)
    set(_argyle_unset_git_redirects
        --unset=GIT_DIR
        --unset=GIT_WORK_TREE
        --unset=GIT_INDEX_FILE
        --unset=GIT_OBJECT_DIRECTORY
        --unset=GIT_ALTERNATE_OBJECT_DIRECTORIES
        --unset=GIT_COMMON_DIR
        --unset=GIT_NAMESPACE
        --unset=GIT_CEILING_DIRECTORIES
        --unset=GIT_DISCOVERY_ACROSS_FILESYSTEM
        --unset=GIT_SHALLOW_FILE
        --unset=GIT_QUARANTINE_PATH)
    execute_process(
        COMMAND "${CMAKE_COMMAND}" -E env ${_argyle_unset_git_redirects}
            "${GIT_EXECUTABLE}" -C "${_argyle_idf_path}" rev-parse --show-toplevel
        RESULT_VARIABLE _argyle_toplevel_status
        OUTPUT_VARIABLE _argyle_sdk_toplevel
        ERROR_QUIET
        OUTPUT_STRIP_TRAILING_WHITESPACE
    )
    if(NOT _argyle_toplevel_status EQUAL 0)
        message(FATAL_ERROR
            "Could not query the configured ESP-IDF Git checkout root; check IDF_PATH, SDK read/traverse permissions and ownership, and Git query setup for the CMake user")
    endif()
    get_filename_component(_argyle_expected_sdk_root "${_argyle_idf_path}" REALPATH)
    get_filename_component(_argyle_actual_sdk_root "${_argyle_sdk_toplevel}" REALPATH)
    if(NOT "${_argyle_actual_sdk_root}" STREQUAL "${_argyle_expected_sdk_root}")
        message(FATAL_ERROR
            "Configured IDF_PATH does not point to the ESP-IDF Git checkout root; set IDF_PATH to the SDK repository root and rerun CMake")
    endif()

    execute_process(
        COMMAND "${CMAKE_COMMAND}" -E env ${_argyle_unset_git_redirects}
            "${GIT_EXECUTABLE}" -C "${_argyle_idf_path}" rev-parse HEAD
        RESULT_VARIABLE _argyle_revision_status
        OUTPUT_VARIABLE _argyle_sdk_revision
        ERROR_QUIET
        OUTPUT_STRIP_TRAILING_WHITESPACE
    )
    if(NOT _argyle_revision_status EQUAL 0 OR NOT _argyle_sdk_revision MATCHES "^[0-9a-fA-F][0-9a-fA-F]+$")
        message(FATAL_ERROR
            "Could not query the configured ESP-IDF Git revision; check SDK checkout permissions and ownership and Git query setup for the CMake user")
    endif()
    string(LENGTH "${_argyle_sdk_revision}" _argyle_revision_length)
    if(NOT _argyle_revision_length EQUAL 40)
        message(FATAL_ERROR
            "The configured ESP-IDF revision must be a 40-character Git commit hash")
    endif()

    set(_argyle_context_dir "${_argyle_build_dir}/argyle-nimble")
    file(MAKE_DIRECTORY "${_argyle_context_dir}")
    set(_argyle_capture "${_argyle_context_dir}/compiler-capture.json")
    if(ARG_OUTPUT)
        set(_argyle_output "${ARG_OUTPUT}")
    else()
        set(_argyle_output "${_argyle_context_dir}/build-context-v1.json")
    endif()
    if(NOT IS_ABSOLUTE "${_argyle_output}")
        message(FATAL_ERROR "build-context OUTPUT must be an absolute path")
    endif()

    set(_argyle_probe_source "${_argyle_context_dir}/context_probe.c")
    file(WRITE "${_argyle_probe_source}" "int argyle_nimble_context_probe(void) { return 0; }\n")
    set(_argyle_probe_target "argyle_nimble_context_probe")
    set(_argyle_export_target "argyle_nimble_export_context")
    if(TARGET "${_argyle_probe_target}" OR TARGET "${_argyle_export_target}")
        message(FATAL_ERROR
            "argyle_nimble_export_build_context may be called only once in a CMake project")
    endif()

    add_library("${_argyle_probe_target}" OBJECT "${_argyle_probe_source}")
    set_target_properties("${_argyle_probe_target}" PROPERTIES EXCLUDE_FROM_ALL TRUE)
    _argyle_nimble_reject_compile_launchers(
        "${ARG_CONSUMER_TARGET}" "${_argyle_probe_target}")

    # Replace the probe's directory-seeded properties with the consumer's
    # effective target properties. Appending with target_* commands would keep
    # directory flags added after the consumer was created, even though the
    # consumer never inherited them. The target-property expressions include
    # transitive usage requirements; no target_link_libraries edge is needed,
    # avoiding a cycle when the consumer depends on Cargo and Cargo depends on
    # this export target. TARGET_GENEX_EVAL resolves expressions in the
    # consumer target's context.
    set_property(TARGET "${_argyle_probe_target}" PROPERTY INCLUDE_DIRECTORIES
        "$<TARGET_GENEX_EVAL:${ARG_CONSUMER_TARGET},$<TARGET_PROPERTY:${ARG_CONSUMER_TARGET},INCLUDE_DIRECTORIES>>")
    set_property(TARGET "${_argyle_probe_target}" PROPERTY COMPILE_DEFINITIONS
        "$<TARGET_GENEX_EVAL:${ARG_CONSUMER_TARGET},$<TARGET_PROPERTY:${ARG_CONSUMER_TARGET},COMPILE_DEFINITIONS>>")
    set_property(TARGET "${_argyle_probe_target}" PROPERTY COMPILE_OPTIONS
        "$<TARGET_GENEX_EVAL:${ARG_CONSUMER_TARGET},$<TARGET_PROPERTY:${ARG_CONSUMER_TARGET},COMPILE_OPTIONS>>")
    set_property(TARGET "${_argyle_probe_target}" PROPERTY COMPILE_FEATURES
        "$<TARGET_GENEX_EVAL:${ARG_CONSUMER_TARGET},$<TARGET_PROPERTY:${ARG_CONSUMER_TARGET},COMPILE_FEATURES>>")

    # Scalar properties do not accept generator expressions as their values.
    # Copy current values now and repeat this at directory-finalize time so
    # top-level CMake can finish configuring the consumer first.
    _argyle_nimble_copy_scalar_compile_properties(
        "${ARG_CONSUMER_TARGET}" "${_argyle_probe_target}")

    # COMPILE_FLAGS is a legacy string property which CMake evaluates when
    # constructing its compiler invocation; leave its tokenization to CMake.
    set_property(TARGET "${_argyle_probe_target}" PROPERTY COMPILE_FLAGS
        "$<TARGET_GENEX_EVAL:${ARG_CONSUMER_TARGET},$<TARGET_PROPERTY:${ARG_CONSUMER_TARGET},COMPILE_FLAGS>>")

    set_property(TARGET "${_argyle_probe_target}" PROPERTY
        C_COMPILER_LAUNCHER
        "${Python3_EXECUTABLE};${CMAKE_CURRENT_FUNCTION_LIST_DIR}/capture_compiler.py;${_argyle_capture}")

    set(_argyle_implicit_include_args)
    foreach(_argyle_include IN LISTS CMAKE_C_IMPLICIT_INCLUDE_DIRECTORIES)
        list(APPEND _argyle_implicit_include_args --implicit-include "${_argyle_include}")
    endforeach()

    add_custom_target("${_argyle_export_target}"
        COMMAND "${Python3_EXECUTABLE}" "${CMAKE_CURRENT_FUNCTION_LIST_DIR}/export_build_context.py"
            --sdk-revision "${_argyle_sdk_revision}"
            --idf-version "${_argyle_idf_version}"
            --sdk-root "${_argyle_idf_path}"
            --build-root "${_argyle_build_dir}"
            --chip "${_argyle_idf_target}"
            --idf-arch "${_argyle_idf_target_arch}"
            --sdkconfig "${_argyle_sdkconfig}"
            --sdkconfig-header "${_argyle_sdkconfig_header}"
            --version-header "${_argyle_idf_path}/components/esp_common/include/esp_idf_version.h"
            --compiler-capture "${_argyle_capture}"
            --output "${_argyle_output}"
            --build-configuration=$<CONFIG>
            ${_argyle_implicit_include_args}
        BYPRODUCTS "${_argyle_output}"
        VERBATIM
        COMMENT "Exporting configured ESP-IDF C build context")
    add_dependencies("${_argyle_export_target}" "${_argyle_probe_target}")

    # Store the target identity because deferred calls run after this
    # function's local variables have gone out of scope. The callback is
    # scheduled in the source root so it sees properties set by later
    # components and top-level CMake code.
    set_property(GLOBAL PROPERTY ARGYLE_NIMBLE_CONTEXT_CONSUMER_TARGET "${ARG_CONSUMER_TARGET}")
    set_property(GLOBAL PROPERTY ARGYLE_NIMBLE_CONTEXT_PROBE_TARGET "${_argyle_probe_target}")
    set_property(GLOBAL PROPERTY ARGYLE_NIMBLE_CONTEXT_PROBE_LAUNCHER
        "${Python3_EXECUTABLE};${CMAKE_CURRENT_FUNCTION_LIST_DIR}/capture_compiler.py;${_argyle_capture}")
    cmake_language(DEFER DIRECTORY "${CMAKE_SOURCE_DIR}"
        CALL _argyle_nimble_finalize_build_context_export)

    set(ARGYLE_NIMBLE_BUILD_CONTEXT_TARGET "${_argyle_export_target}" PARENT_SCOPE)
    set(ARGYLE_NIMBLE_BUILD_CONTEXT_FILE "${_argyle_output}" PARENT_SCOPE)
endfunction()
