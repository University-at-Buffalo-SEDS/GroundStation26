cmake_minimum_required(VERSION 3.22)
project(sedsprintf_rs_cmake NONE)

# --- Find Python interpreter ---
find_package(Python3 COMPONENTS Interpreter REQUIRED)
set(PYTHON_EXECUTABLE "${Python3_EXECUTABLE}")

set(SEDSPRINTF_RS_DIR_DEFAULT "${CMAKE_CURRENT_LIST_DIR}/")
set(SEDSPRINTF_RS_DIR "${SEDSPRINTF_RS_DIR_DEFAULT}" CACHE PATH "Path to sedsprintf_rs crate root")
get_filename_component(RUST_DIR "${SEDSPRINTF_RS_DIR}" REALPATH)

# Detect embedded build
# Option: allow user/parent to force embedded on/off
option(SEDSPRINTF_EMBEDDED_BUILD "Build sedsprintf_rs for embedded" OFF)

# Auto-detect embedded if user hasn't forced it
if (NOT SEDSPRINTF_EMBEDDED_BUILD)
    if (CMAKE_C_COMPILER MATCHES "arm-none-eabi"
            OR CMAKE_TOOLCHAIN_FILE MATCHES "stm32"
            OR CMAKE_TOOLCHAIN_FILE MATCHES "gcc-arm-none-eabi"
            OR DEFINED ENV{STM32CUBEIDE_DIR}
    )
        set(SEDSPRINTF_EMBEDDED_BUILD ON CACHE BOOL "Build sedsprintf_rs for embedded" FORCE)
    endif ()
endif ()


# ============================================================
# Rust target triple selection
# - Default uses current "mode" (host vs STM)
# - Can be overridden upstream with:
#     set(SEDSPRINTF_RS_TARGET "thumbv7m-none-eabi" CACHE STRING "" FORCE)
#   *before* add_subdirectory(...)
# ============================================================
if (NOT DEFINED SEDSPRINTF_RS_TARGET)
    if (SEDSPRINTF_EMBEDDED_BUILD)
        # Default embedded triple
        set(SEDSPRINTF_RS_TARGET "thumbv7em-none-eabihf" CACHE STRING "Rust target triple for sedsprintf_rs (empty = host default)" )
    else ()
        # Host build: empty means Cargo's default host triple
        set(SEDSPRINTF_RS_TARGET "" CACHE STRING "Rust target triple for sedsprintf_rs (empty = host default)" )
    endif ()
endif ()

set(RUST_TRIPLE "${SEDSPRINTF_RS_TARGET}")

# Figure out Rust output paths depending on whether we use a cross target
set(RUST_PROFILE "$<IF:$<CONFIG:Debug>,debug,release>")
# If embedded it is release-embedded otherwise release
set(RELEASE_PROFILE_DIR "release")
if (SEDSPRINTF_EMBEDDED_BUILD)
    set(RELEASE_PROFILE_DIR "release-embedded")
endif ()

if (RUST_TRIPLE STREQUAL "")
    # Host build: Cargo outputs target/debug / target/release
    set(RS_LIB_DBG "${RUST_DIR}/target/debug/libsedsprintf_rs_2026.a")
    set(RS_LIB_REL "${RUST_DIR}/target/release/libsedsprintf_rslibsedsprintf_rs_2026.a")
else ()
    # Cross build: Cargo outputs target/<triple>/debug / target/<triple>/release
    set(RS_LIB_DBG "${RUST_DIR}/target/${RUST_TRIPLE}/debug/libsedsprintf_rs_2026.a")
    set(RS_LIB_REL "${RUST_DIR}/target/${RUST_TRIPLE}/${RELEASE_PROFILE_DIR}/libsedsprintf_rs_2026.a")
endif ()

set(RS_INCLUDE "${RUST_DIR}/C-Headers")
set(RS_HEADER "${RS_INCLUDE}/sedsprintf.h")

# --- Build-time args (with generator expression) ---
set(RS_ARGS_STR "")
if (SEDSPRINTF_EMBEDDED_BUILD)
    set(RS_ARGS_STR "${RS_ARGS_STR} embedded")
endif ()
# Add target=... when a non-empty triple is used (for logging / info only)
if (NOT RUST_TRIPLE STREQUAL "")
    set(RS_ARGS_STR "${RS_ARGS_STR} target=${RUST_TRIPLE}")
endif ()
set(RS_ARG_REL " $<$<CONFIG:Release>:release>")
set(RS_ARGS_STR "${RS_ARGS_STR} ${RS_ARG_REL}")

# --- Configure-time bootstrap args (NO generator expressions) ---
set(RS_ARGS_BOOTSTRAP "")
if (SEDSPRINTF_EMBEDDED_BUILD)
    list(APPEND RS_ARGS_BOOTSTRAP "embedded")
endif ()
if (NOT RUST_TRIPLE STREQUAL "")
    list(APPEND RS_ARGS_BOOTSTRAP "target=${RUST_TRIPLE}")
endif ()
if (CMAKE_BUILD_TYPE STREQUAL "Release")
    # For single-config generators like Makefile/Ninja
    list(APPEND RS_ARGS_BOOTSTRAP "release")
endif ()

# Ensure the include directory exists (header may be generated later)
file(MAKE_DIRECTORY "${RS_INCLUDE}")

# If the header is missing at configure time, try building once to generate it.
if (NOT EXISTS "${RS_HEADER}")
    message(STATUS "sedsprintf_rs: header not found (${RS_HEADER}); attempting to build once to generate it…")
    execute_process(
        COMMAND "${PYTHON_EXECUTABLE}" build.py ${RS_ARGS_BOOTSTRAP}
        WORKING_DIRECTORY "${RUST_DIR}"
        RESULT_VARIABLE _hdr_build_res
    )
    if (NOT _hdr_build_res EQUAL 0)
        message(WARNING "Initial build to generate header failed (exit ${_hdr_build_res}); will rely on custom build step.")
    endif ()
endif ()

# Track inputs
file(GLOB_RECURSE RS_SRC CONFIGURE_DEPENDS
    "${RUST_DIR}/src/*.rs"
    "${RUST_DIR}/build.rs"
    "${RUST_DIR}/Cargo.toml"
    "${RUST_DIR}/Cargo.lock"
)

# Build rule: produce libs AND the header (generator expressions OK here)
add_custom_command(
    OUTPUT
        "${RS_LIB_DBG}" "${RS_LIB_REL}" "${RS_HEADER}"
    COMMAND ${CMAKE_COMMAND} -E chdir "${RUST_DIR}" "${PYTHON_EXECUTABLE}" build.py ${RS_ARGS_BOOTSTRAP}
    DEPENDS ${RS_SRC}
    COMMENT "Building sedsprintf_rs (${RUST_PROFILE}) for ${RUST_TRIPLE}"
    USES_TERMINAL
    VERBATIM
)

add_custom_target(sedsprintf_rs_build
    DEPENDS
        "${RS_LIB_DBG}" "${RS_LIB_REL}" "${RS_HEADER}"
)

# Imported library the C/C++ links against
add_library(sedsprintf_rs STATIC IMPORTED GLOBAL)
set_target_properties(sedsprintf_rs PROPERTIES
    IMPORTED_LOCATION_DEBUG "${RS_LIB_DBG}"
    IMPORTED_LOCATION_RELEASE "${RS_LIB_REL}"
    INTERFACE_INCLUDE_DIRECTORIES "${RS_INCLUDE}"
)
add_dependencies(sedsprintf_rs sedsprintf_rs_build)

include(CheckLinkerFlag)
include(CheckCCompilerFlag)
check_linker_flag(C "-Wl,-z,noexecstack" HAVE_LD_NOEXECSTACK)
check_c_compiler_flag("-Wa,--noexecstack" HAVE_AS_NOEXECSTACK)

if (SEDSPRINTF_EMBEDDED_BUILD)
    target_compile_options(sedsprintf_rs INTERFACE -Wa,--noexecstack)
    target_link_options(sedsprintf_rs INTERFACE -Wl,-z,noexecstack)
endif ()

add_compile_options(-ffunction-sections -fdata-sections -Os)
add_link_options(-Wl,--gc-sections -Wl,--icf=all)

add_library(sedsprintf_rs::sedsprintf_rs ALIAS sedsprintf_rs)
set(SEDSPRINTF_RS_LIB_DEBUG "${RS_LIB_DBG}" PARENT_SCOPE)
set(SEDSPRINTF_RS_LIB_RELEASE "${RS_LIB_REL}" PARENT_SCOPE)
set(SEDSPRINTF_RS_INCLUDE_DIR "${RS_INCLUDE}" PARENT_SCOPE)

message(STATUS "sedsprintf_rs include dir: ${RS_INCLUDE}")
message(STATUS "sedsprintf_rs debug lib:  ${RS_LIB_DBG}")
message(STATUS "sedsprintf_rs release lib:${RS_LIB_REL}")
