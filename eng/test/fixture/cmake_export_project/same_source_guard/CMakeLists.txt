get_property(_argyle_guard_seen GLOBAL PROPERTY ARGYLE_SAME_SOURCE_GUARD_SEEN)
if(NOT _argyle_guard_seen)
    add_library(argyle_same_source_guard_consumer OBJECT guard.c)
    set_property(GLOBAL PROPERTY ARGYLE_SAME_SOURCE_GUARD_SEEN TRUE)
else()
    add_library(argyle_same_source_guard_consumer_second OBJECT guard.c)
    include("${ARGYLE_NIMBLE_CMAKE_MODULE}")
    # The target and this CMakeLists have the same source directory, while the
    # target belongs to the first binary directory added above.
    argyle_nimble_export_build_context(
        CONSUMER_TARGET argyle_same_source_guard_consumer)
endif()
