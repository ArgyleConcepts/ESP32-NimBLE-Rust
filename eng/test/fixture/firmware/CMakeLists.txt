# Generic ESP-IDF compile/link fixture for argyle-nimble's Cargo integration.
# The validation driver copies this template into an isolated job directory.
cmake_minimum_required(VERSION 3.22)
include($ENV{IDF_PATH}/tools/cmake/project.cmake)
project(argyle_nimble_link_fixture)
