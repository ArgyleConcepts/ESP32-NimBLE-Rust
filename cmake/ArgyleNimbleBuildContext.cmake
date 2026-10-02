include_guard(GLOBAL)

# Export the actual configured consumer C context through a one-source probe.
# The compile launcher records argv as process arguments; no command text is
# split or reinterpreted by a shell.
function(argyle_nimble_export_build_context)
    cmake_parse_arguments(PARSE_ARGV 0 ARG "" "CONSUMER_TARGET;OUTPUT" "")

    if(NOT ARG_CONSUMER_TARGET OR NOT TARGET "${ARG_CONSUMER_TARGET}")
        message(FATAL_ERROR
            "argyle_nimble_export_build_context requires CONSUMER_TARGET to name an existing configured CMake target")
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
    idf_build_get_property(_argyle_build_dir BUILD_DIR)
    if(NOT _argyle_idf_path OR NOT _argyle_idf_target OR NOT _argyle_idf_target_arch
        OR NOT _argyle_idf_version OR NOT _argyle_sdkconfig OR NOT _argyle_build_dir)
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

    find_package(Python3 REQUIRED COMPONENTS Interpreter)
    find_package(Git REQUIRED)
    execute_process(
        COMMAND "${GIT_EXECUTABLE}" -C "${_argyle_idf_path}" rev-parse HEAD
        RESULT_VARIABLE _argyle_revision_status
        OUTPUT_VARIABLE _argyle_sdk_revision
        ERROR_QUIET
        OUTPUT_STRIP_TRAILING_WHITESPACE
    )
    if(NOT _argyle_revision_status EQUAL 0 OR NOT _argyle_sdk_revision MATCHES "^[0-9a-fA-F][0-9a-fA-F]+$")
        message(FATAL_ERROR
            "Could not determine the configured ESP-IDF checkout revision; use a complete SDK checkout and rerun CMake")
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

    # Copy the consumer's private compile properties. CMake's corresponding
    # TARGET_PROPERTY expressions include transitive usage requirements. Do
    # not add a target_link_libraries edge: a consumer may depend on Cargo,
    # while Cargo depends on this export target, which would form a cycle.
    # TARGET_GENEX_EVAL resolves configured generator expressions in the
    # consumer target's context.
    target_include_directories("${_argyle_probe_target}" PRIVATE
        "$<TARGET_GENEX_EVAL:${ARG_CONSUMER_TARGET},$<TARGET_PROPERTY:${ARG_CONSUMER_TARGET},INCLUDE_DIRECTORIES>>")
    target_compile_definitions("${_argyle_probe_target}" PRIVATE
        "$<TARGET_GENEX_EVAL:${ARG_CONSUMER_TARGET},$<TARGET_PROPERTY:${ARG_CONSUMER_TARGET},COMPILE_DEFINITIONS>>")
    target_compile_options("${_argyle_probe_target}" PRIVATE
        "$<TARGET_GENEX_EVAL:${ARG_CONSUMER_TARGET},$<TARGET_PROPERTY:${ARG_CONSUMER_TARGET},COMPILE_OPTIONS>>")
    target_compile_features("${_argyle_probe_target}" PRIVATE
        "$<TARGET_GENEX_EVAL:${ARG_CONSUMER_TARGET},$<TARGET_PROPERTY:${ARG_CONSUMER_TARGET},COMPILE_FEATURES>>")

    # Scalar properties such as C_STANDARD do not accept generator expressions
    # as their values. Copy their configured values directly and skip unset
    # properties. Shared/module targets imply PIC even when the property was
    # never explicitly initialized.
    get_target_property(_argyle_consumer_type "${ARG_CONSUMER_TARGET}" TYPE)
    foreach(_argyle_property IN ITEMS C_STANDARD C_STANDARD_REQUIRED C_EXTENSIONS
        POSITION_INDEPENDENT_CODE C_VISIBILITY_PRESET)
        get_target_property(_argyle_value "${ARG_CONSUMER_TARGET}" "${_argyle_property}")
        if(NOT _argyle_value STREQUAL "_argyle_value-NOTFOUND")
            set_property(TARGET "${_argyle_probe_target}" PROPERTY "${_argyle_property}" "${_argyle_value}")
        elseif(_argyle_property STREQUAL "POSITION_INDEPENDENT_CODE"
            AND (_argyle_consumer_type STREQUAL "SHARED_LIBRARY"
                OR _argyle_consumer_type STREQUAL "MODULE_LIBRARY"))
            set_property(TARGET "${_argyle_probe_target}" PROPERTY POSITION_INDEPENDENT_CODE TRUE)
        endif()
    endforeach()

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
            --sdkconfig-header "${_argyle_build_dir}/config/sdkconfig.h"
            --version-header "${_argyle_idf_path}/components/esp_common/include/esp_idf_version.h"
            --compiler-capture "${_argyle_capture}"
            --output "${_argyle_output}"
            --build-configuration "$<CONFIG>"
            ${_argyle_implicit_include_args}
        BYPRODUCTS "${_argyle_output}"
        VERBATIM
        COMMENT "Exporting configured ESP-IDF C build context")
    add_dependencies("${_argyle_export_target}" "${_argyle_probe_target}")

    set(ARGYLE_NIMBLE_BUILD_CONTEXT_TARGET "${_argyle_export_target}" PARENT_SCOPE)
    set(ARGYLE_NIMBLE_BUILD_CONTEXT_FILE "${_argyle_output}" PARENT_SCOPE)
endfunction()
