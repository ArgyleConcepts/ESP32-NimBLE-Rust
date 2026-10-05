include_guard(GLOBAL)

# Reusable ESP-IDF integration for a Rust static library that depends on
# argyle-nimble. idf.py owns the firmware build: this module exports the
# configured C build context, runs Cargo for the IDF target through a custom
# target, and links the resulting static library into an existing component.
# It never configures or builds a second ESP-IDF project.
#
# Locate this file through Cargo metadata (see docs/IDF_INTEGRATION.md) so the
# integration comes from the resolved argyle-nimble package, not a checkout.

include("${CMAKE_CURRENT_LIST_DIR}/ArgyleNimbleBuildContext.cmake")

set(_ARGYLE_NIMBLE_CARGO_MODULE_DIR "${CMAKE_CURRENT_LIST_DIR}")

# argyle_nimble_add_cargo_staticlib(
#     CONSUMER_TARGET <component library, normally ${COMPONENT_LIB}>
#     MANIFEST_PATH   <absolute path to the application's Cargo.toml>
#     LIBRARY_NAME    <Rust library name; Cargo produces lib<name>.a>
#     [PACKAGE <Cargo package to build in a workspace>]
#     [FEATURES <feature>...]
#     [REQUIRES <additional IDF components the Rust code links against>...]
#     [RUST_TOOLCHAIN <rustup toolchain name>]
#     [LOCKED] [OFFLINE]
#     [LINK_AUDIT])
#
# Call it from the consuming component's CMakeLists.txt after
# idf_component_register(). Cargo, Espressif clang, and libclang are selected
# with the cache variables ARGYLE_NIMBLE_CARGO, ARGYLE_NIMBLE_ESP_CLANG, and
# ARGYLE_NIMBLE_LIBCLANG_PATH, or the ARGYLE_NIMBLE_ESP_CLANG and LIBCLANG_PATH
# environment variables at configure time. LINK_AUDIT is for validation
# fixtures: it retains an uncalled root that references every bound C symbol.
function(argyle_nimble_add_cargo_staticlib)
    cmake_parse_arguments(PARSE_ARGV 0 ARG "LOCKED;OFFLINE;LINK_AUDIT"
        "CONSUMER_TARGET;MANIFEST_PATH;LIBRARY_NAME;PACKAGE;RUST_TOOLCHAIN"
        "FEATURES;REQUIRES")
    if(ARG_UNPARSED_ARGUMENTS)
        message(FATAL_ERROR
            "argyle_nimble_add_cargo_staticlib received unknown arguments: ${ARG_UNPARSED_ARGUMENTS}")
    endif()
    if(NOT ARG_CONSUMER_TARGET OR NOT TARGET "${ARG_CONSUMER_TARGET}")
        message(FATAL_ERROR
            "argyle_nimble_add_cargo_staticlib requires CONSUMER_TARGET to name the configured component library")
    endif()
    if(NOT ARG_MANIFEST_PATH OR NOT IS_ABSOLUTE "${ARG_MANIFEST_PATH}"
        OR NOT EXISTS "${ARG_MANIFEST_PATH}")
        message(FATAL_ERROR
            "argyle_nimble_add_cargo_staticlib requires MANIFEST_PATH to name an existing absolute Cargo.toml")
    endif()
    if(NOT ARG_LIBRARY_NAME MATCHES "^[A-Za-z0-9_]+$")
        message(FATAL_ERROR
            "argyle_nimble_add_cargo_staticlib requires LIBRARY_NAME to be the Rust library name (letters, digits, underscores)")
    endif()
    if(TARGET argyle_nimble_cargo_build)
        message(FATAL_ERROR
            "argyle_nimble_add_cargo_staticlib may be called only once in an ESP-IDF project")
    endif()
    if(NOT ESP_PLATFORM OR NOT COMMAND idf_build_get_property)
        message(FATAL_ERROR
            "argyle_nimble_add_cargo_staticlib must run inside a configured ESP-IDF CMake project")
    endif()

    idf_build_get_property(_argyle_idf_target IDF_TARGET)
    idf_build_get_property(_argyle_build_dir BUILD_DIR)
    idf_build_get_property(_argyle_python PYTHON)
    if(_argyle_idf_target STREQUAL "esp32c3")
        set(_argyle_rust_target "riscv32imc-esp-espidf")
    elseif(_argyle_idf_target STREQUAL "esp32s3")
        set(_argyle_rust_target "xtensa-esp32s3-espidf")
    else()
        message(FATAL_ERROR
            "argyle-nimble supports ESP-IDF targets esp32c3 and esp32s3; the configured target is ${_argyle_idf_target}")
    endif()

    # Initial support is std with ESP-IDF Newlib. The build script checks the
    # same configuration again before generating target artifacts.
    if(CONFIG_LIBC_PICOLIBC OR NOT CONFIG_LIBC_NEWLIB)
        message(FATAL_ERROR
            "argyle-nimble requires CONFIG_LIBC_NEWLIB=y; Picolibc and other C libraries are not supported. Set it in sdkconfig.defaults and reconfigure")
    endif()
    if(NOT CONFIG_BT_ENABLED OR NOT CONFIG_BT_NIMBLE_ENABLED)
        message(FATAL_ERROR
            "argyle-nimble requires CONFIG_BT_ENABLED=y and CONFIG_BT_NIMBLE_ENABLED=y")
    endif()

    # Match Rust code generation to the IDF compiler optimization choice. Rust
    # has no -Og; opt-level 1 is the closest debug-friendly setting.
    if(CONFIG_COMPILER_OPTIMIZATION_DEBUG)
        set(_argyle_profile "dev")
        set(_argyle_opt_level "1")
    elseif(CONFIG_COMPILER_OPTIMIZATION_NONE)
        set(_argyle_profile "dev")
        set(_argyle_opt_level "0")
    elseif(CONFIG_COMPILER_OPTIMIZATION_SIZE)
        set(_argyle_profile "release")
        set(_argyle_opt_level "s")
    elseif(CONFIG_COMPILER_OPTIMIZATION_PERF)
        set(_argyle_profile "release")
        set(_argyle_opt_level "2")
    else()
        message(FATAL_ERROR
            "No supported ESP-IDF compiler optimization level is selected (DEBUG, NONE, SIZE, or PERF)")
    endif()
    if(_argyle_profile STREQUAL "dev")
        set(_argyle_profile_dir "debug")
    else()
        set(_argyle_profile_dir "release")
    endif()

    find_program(ARGYLE_NIMBLE_CARGO cargo
        HINTS "$ENV{CARGO_HOME}/bin" "$ENV{HOME}/.cargo/bin"
        DOC "Cargo executable used to build the ESP-IDF Rust library")
    if(NOT ARGYLE_NIMBLE_CARGO)
        message(FATAL_ERROR
            "Cargo was not found; install the pinned Rust toolchain or set ARGYLE_NIMBLE_CARGO")
    endif()
    if(NOT ARGYLE_NIMBLE_ESP_CLANG AND DEFINED ENV{ARGYLE_NIMBLE_ESP_CLANG})
        set(ARGYLE_NIMBLE_ESP_CLANG "$ENV{ARGYLE_NIMBLE_ESP_CLANG}" CACHE FILEPATH
            "Espressif clang used for private NimBLE binding generation")
    endif()
    if(NOT ARGYLE_NIMBLE_LIBCLANG_PATH AND DEFINED ENV{LIBCLANG_PATH})
        set(ARGYLE_NIMBLE_LIBCLANG_PATH "$ENV{LIBCLANG_PATH}" CACHE FILEPATH
            "libclang matching ARGYLE_NIMBLE_ESP_CLANG")
    endif()
    foreach(_argyle_tool IN ITEMS ARGYLE_NIMBLE_ESP_CLANG ARGYLE_NIMBLE_LIBCLANG_PATH)
        if(NOT ${_argyle_tool} OR NOT IS_ABSOLUTE "${${_argyle_tool}}"
            OR NOT EXISTS "${${_argyle_tool}}")
            message(FATAL_ERROR
                "${_argyle_tool} must name the pinned ESP-IDF esp-clang package file; the selected value is missing or not absolute")
        endif()
    endforeach()

    argyle_nimble_export_build_context(CONSUMER_TARGET "${ARG_CONSUMER_TARGET}")

    # Keep Cargo output inside this IDF build directory and separate it by
    # chip, so a target switch or fullclean never reuses another target's state.
    set(_argyle_target_dir "${_argyle_build_dir}/argyle-nimble/cargo/${_argyle_idf_target}")
    set(_argyle_library
        "${_argyle_target_dir}/${_argyle_rust_target}/${_argyle_profile_dir}/lib${ARG_LIBRARY_NAME}.a")
    set(_argyle_identity "${_argyle_build_dir}/argyle-nimble/cargo-integration.json")

    set(_argyle_driver_args
        --cargo "${ARGYLE_NIMBLE_CARGO}"
        --manifest-path "${ARG_MANIFEST_PATH}"
        --library-name "${ARG_LIBRARY_NAME}"
        --chip "${_argyle_idf_target}"
        --rust-target "${_argyle_rust_target}"
        --profile "${_argyle_profile}"
        --opt-level "${_argyle_opt_level}"
        --target-dir "${_argyle_target_dir}"
        --context "${ARGYLE_NIMBLE_BUILD_CONTEXT_FILE}"
        --esp-clang "${ARGYLE_NIMBLE_ESP_CLANG}"
        --libclang "${ARGYLE_NIMBLE_LIBCLANG_PATH}"
        --identity "${_argyle_identity}")
    if(ARG_PACKAGE)
        list(APPEND _argyle_driver_args --package "${ARG_PACKAGE}")
    endif()
    if(ARG_RUST_TOOLCHAIN)
        list(APPEND _argyle_driver_args --toolchain "${ARG_RUST_TOOLCHAIN}")
    endif()
    foreach(_argyle_feature IN LISTS ARG_FEATURES)
        list(APPEND _argyle_driver_args --feature "${_argyle_feature}")
    endforeach()
    foreach(_argyle_flag IN ITEMS LOCKED OFFLINE LINK_AUDIT)
        if(ARG_${_argyle_flag})
            string(TOLOWER "${_argyle_flag}" _argyle_option)
            string(REPLACE "_" "-" _argyle_option "${_argyle_option}")
            list(APPEND _argyle_driver_args "--${_argyle_option}")
        endif()
    endforeach()

    # Cargo tracks its own inputs, so the target always runs and Cargo decides
    # what is fresh. Ninja relinks only when the library actually changes.
    add_custom_target(argyle_nimble_cargo_build
        COMMAND "${_argyle_python}" "${_ARGYLE_NIMBLE_CARGO_MODULE_DIR}/argyle_nimble_cargo.py"
            ${_argyle_driver_args}
        BYPRODUCTS "${_argyle_library}" "${_argyle_identity}"
        WORKING_DIRECTORY "${_argyle_build_dir}"
        USES_TERMINAL
        VERBATIM
        COMMENT "Building Rust library ${ARG_LIBRARY_NAME} for ${_argyle_rust_target}")
    add_dependencies(argyle_nimble_cargo_build "${ARGYLE_NIMBLE_BUILD_CONTEXT_TARGET}")

    # Rust std and the private bindings reference these IDF components.
    set(_argyle_requires bt esp_libc pthread freertos esp_system esp_hw_support ${ARG_REQUIRES})
    list(REMOVE_DUPLICATES _argyle_requires)
    add_prebuilt_library(argyle_nimble_rust_library "${_argyle_library}"
        PRIV_REQUIRES ${_argyle_requires})
    add_dependencies(argyle_nimble_rust_library argyle_nimble_cargo_build)
    target_link_libraries("${ARG_CONSUMER_TARGET}" PRIVATE argyle_nimble_rust_library)

    if(ARG_LINK_AUDIT)
        # Propagates to the firmware executable that links this component.
        target_link_options("${ARG_CONSUMER_TARGET}" INTERFACE
            "-Wl,--undefined=argyle_nimble_link_audit")
    endif()

    set(ARGYLE_NIMBLE_RUST_LIBRARY "${_argyle_library}" PARENT_SCOPE)
    set(ARGYLE_NIMBLE_CARGO_IDENTITY "${_argyle_identity}" PARENT_SCOPE)
    set(ARGYLE_NIMBLE_BUILD_CONTEXT_FILE "${ARGYLE_NIMBLE_BUILD_CONTEXT_FILE}" PARENT_SCOPE)
endfunction()
