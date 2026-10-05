set(fixture_sources "main.c")
if(ARGYLE_NIMBLE_FIXTURE_APP_LSTAT_SHIM)
    list(APPEND fixture_sources "app_lstat_shim.c")
endif()
idf_component_register(SRCS ${fixture_sources} PRIV_REQUIRES bt)
if(ARGYLE_NIMBLE_FIXTURE_APP_LSTAT_SHIM)
    # The component archive is scanned before the Rust library that needs
    # lstat, so the application retains its own shim explicitly.
    target_link_options(${COMPONENT_LIB} INTERFACE "-Wl,--undefined=lstat")
endif()

set(rust_manifest "${CMAKE_CURRENT_LIST_DIR}/../rust/Cargo.toml")

# Locate argyle-nimble through Cargo's resolved package graph rather than a
# checkout path. This is the documented consumer snippet.
find_program(ARGYLE_NIMBLE_CARGO cargo HINTS "$ENV{CARGO_HOME}/bin" "$ENV{HOME}/.cargo/bin")
execute_process(
    COMMAND "${ARGYLE_NIMBLE_CARGO}" metadata --format-version 1 --locked --offline
        --manifest-path "${rust_manifest}"
    WORKING_DIRECTORY "${CMAKE_CURRENT_LIST_DIR}/../rust"
    OUTPUT_VARIABLE cargo_metadata
    RESULT_VARIABLE cargo_metadata_status)
if(NOT cargo_metadata_status EQUAL 0)
    message(FATAL_ERROR "cargo metadata failed for ${rust_manifest}")
endif()
string(JSON package_count LENGTH "${cargo_metadata}" packages)
math(EXPR last_package "${package_count} - 1")
foreach(index RANGE ${last_package})
    string(JSON package_name GET "${cargo_metadata}" packages ${index} name)
    if(package_name STREQUAL "argyle-nimble")
        string(JSON argyle_nimble_manifest GET "${cargo_metadata}" packages ${index} manifest_path)
    endif()
endforeach()
if(NOT argyle_nimble_manifest)
    message(FATAL_ERROR "argyle-nimble is not a dependency of ${rust_manifest}")
endif()
get_filename_component(argyle_nimble_dir "${argyle_nimble_manifest}" DIRECTORY)
include("${argyle_nimble_dir}/cmake/ArgyleNimbleCargo.cmake")

# Validation-only fixture switches for the compatibility-shim audit.
set(fixture_features)
if(ARGYLE_NIMBLE_FIXTURE_STD_AUDIT)
    list(APPEND fixture_features std-audit)
    target_compile_definitions(${COMPONENT_LIB} PRIVATE ARGYLE_NIMBLE_FIXTURE_STD_AUDIT)
endif()
if(ARGYLE_NIMBLE_FIXTURE_LSTAT_AUDIT)
    list(APPEND fixture_features lstat-audit)
    target_compile_definitions(${COMPONENT_LIB} PRIVATE ARGYLE_NIMBLE_FIXTURE_LSTAT_AUDIT)
endif()

argyle_nimble_add_cargo_staticlib(
    CONSUMER_TARGET ${COMPONENT_LIB}
    MANIFEST_PATH "${rust_manifest}"
    LIBRARY_NAME argyle_nimble_link_fixture
    FEATURES ${fixture_features}
    LOCKED
    OFFLINE
    LINK_AUDIT)
