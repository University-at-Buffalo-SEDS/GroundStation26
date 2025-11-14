cmake_minimum_required(VERSION 3.22)
project(c_system_test C)

set(CMAKE_C_STANDARD 11)

# 1) Define the target first
add_executable(c_system_test
        src/main.c
        src/telemetry_sim.c
)

# 2) Local includes (your own headers)
target_include_directories(c_system_test PRIVATE
        "${CMAKE_CURRENT_SOURCE_DIR}/include"
)

# 3) Bring in the Rust crate (parent dir) with an explicit build dir
get_filename_component(SEDSPRINTF_RS_ROOT "${CMAKE_CURRENT_LIST_DIR}/.." REALPATH)
add_subdirectory(
        "${SEDSPRINTF_RS_ROOT}"
        "${CMAKE_CURRENT_BINARY_DIR}/sedsprintf_rs-build"
        EXCLUDE_FROM_ALL
)

# 4) Link + add the crate's public headers
target_link_libraries(c_system_test PRIVATE sedsprintf_rs)
target_include_directories(c_system_test PRIVATE "${SEDSPRINTF_RS_INCLUDE_DIR}")
