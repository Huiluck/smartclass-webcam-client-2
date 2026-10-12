# The Flutter tooling requires that developers have CMake 3.10 or later
# installed. You should not increase this version, as doing so will cause
# the plugin to fail to compile for some customers of the plugin.
cmake_minimum_required(VERSION 3.10)
cmake_policy(SET CMP0079 NEW)

# Project-level configuration.
set(PROJECT_NAME "rust_lib")
project(${PROJECT_NAME} LANGUAGES CXX)

# This value is used when generating builds using this plugin, so it must
# not be changed.
set(PLUGIN_NAME "rust_lib_plugin")

# The Rust library behind the plugin — the bridge, the capture stack and the texture path's C ABI
# (`app/rust_lib/src/lib.rs`) — built by cargokit.
include("../cargokit/cmake/cargokit.cmake")

# The plugin: the `scwc_preview` channel and the pixel-buffer texture that paints the live preview.
list(APPEND PLUGIN_SOURCES
  "scwc_preview_plugin.cc"
)

# Define the plugin library target. Its name must not be changed (see comment
# on PLUGIN_NAME above).
add_library(${PLUGIN_NAME} SHARED
  ${PLUGIN_SOURCES}
)

# Apply a standard set of build settings that are configured in the
# application-level CMakeLists.txt. This can be removed for plugins that want
# full control over build settings.
apply_standard_settings(${PLUGIN_NAME})

# Symbols are hidden by default to reduce the chance of accidental conflicts
# between plugins. This should not be removed; any symbols that should be
# exported should be explicitly exported with the FLUTTER_PLUGIN_EXPORT macro.
set_target_properties(${PLUGIN_NAME} PROPERTIES
  CXX_VISIBILITY_PRESET hidden)
target_compile_definitions(${PLUGIN_NAME} PRIVATE FLUTTER_PLUGIN_IMPL)

# Source include directories and library dependencies. Add any plugin-specific
# dependencies here.
target_include_directories(${PLUGIN_NAME} INTERFACE
  "${CMAKE_CURRENT_SOURCE_DIR}/include")
target_link_libraries(${PLUGIN_NAME} PRIVATE flutter)
target_link_libraries(${PLUGIN_NAME} PRIVATE PkgConfig::GTK)

# The plugin resolves the texture path's C ABI against that library, which is also the one the
# application loads: the frames it paints are the ones the bridge would have carried.
apply_cargokit(${PLUGIN_NAME} ../../rust_lib rust_lib "scwc_preview_latest_rgba")

# The application reaches the bridge through the same library.
target_link_libraries(${BINARY_NAME} PRIVATE "${${PLUGIN_NAME}_cargokit_lib}")
# Both are installed into the bundle's `lib`, so the plugin finds its library next to itself.
set_target_properties(${PLUGIN_NAME} PROPERTIES
  INSTALL_RPATH "$ORIGIN")

# List of absolute paths to libraries that should be bundled with the plugin.
# This list could contain prebuilt libraries, or libraries created by an
# external build triggered from this build file.
set(rust_lib_bundled_libraries
  "${${PLUGIN_NAME}_cargokit_lib}"
  PARENT_SCOPE
)
