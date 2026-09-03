cmake_minimum_required(VERSION 3.16)
project(bluetooth_audio_bridge_audio LANGUAGES CXX)

find_package(PkgConfig REQUIRED)
pkg_check_modules(PIPEWIRE REQUIRED IMPORTED_TARGET libpipewire-0.3>=0.3.48)

add_library(bluetooth_audio_bridge_audio STATIC src/engine.cpp)
target_compile_features(bluetooth_audio_bridge_audio PRIVATE cxx_std_17)
target_include_directories(bluetooth_audio_bridge_audio PUBLIC include)
target_link_libraries(bluetooth_audio_bridge_audio PUBLIC PkgConfig::PIPEWIRE)
set_target_properties(bluetooth_audio_bridge_audio PROPERTIES
    POSITION_INDEPENDENT_CODE ON
    ARCHIVE_OUTPUT_DIRECTORY "${CMAKE_BINARY_DIR}/lib")
