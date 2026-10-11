cmake_minimum_required(VERSION 3.5)
project(gridpin_ext)
set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

file(STRINGS "${CMAKE_CURRENT_SOURCE_DIR}/Cargo.toml" GRIDPIN_VERSION_LINE REGEX "^version =")
string(REGEX REPLACE "^version = \"([^\"]+)\".*" "\\1" GRIDPIN_VERSION "${GRIDPIN_VERSION_LINE}")
set(GRIDPIN_CARGO_TARGET_DIR "${CMAKE_BINARY_DIR}/rust" CACHE PATH "Cargo build cache")
find_program(GRIDPIN_CARGO cargo REQUIRED)
set(GRIDPIN_RUST_TARGET_ARGS)
set(GRIDPIN_RUST_OUTPUT "${GRIDPIN_CARGO_TARGET_DIR}/release")
if(Rust_CARGO_TARGET)
  list(APPEND GRIDPIN_RUST_TARGET_ARGS --target "${Rust_CARGO_TARGET}")
  set(GRIDPIN_RUST_OUTPUT "${GRIDPIN_CARGO_TARGET_DIR}/${Rust_CARGO_TARGET}/release")
endif()
if(WIN32 AND (NOT Rust_CARGO_TARGET OR Rust_CARGO_TARGET MATCHES "msvc"))
  set(GRIDPIN_RUST_LIBRARY "${GRIDPIN_RUST_OUTPUT}/gridpin_ext.lib")
else()
  set(GRIDPIN_RUST_LIBRARY "${GRIDPIN_RUST_OUTPUT}/libgridpin_ext.a")
endif()

# Cargo checks the Rust dependency graph even when C++ inputs have not changed.
add_custom_target(gridpin_rust
  COMMAND "${GRIDPIN_CARGO}" build --release --locked
    --manifest-path "${CMAKE_CURRENT_SOURCE_DIR}/Cargo.toml"
    --target-dir "${GRIDPIN_CARGO_TARGET_DIR}" ${GRIDPIN_RUST_TARGET_ARGS}
  WORKING_DIRECTORY "${CMAKE_CURRENT_SOURCE_DIR}"
  BYPRODUCTS "${GRIDPIN_RUST_LIBRARY}"
  VERBATIM)
include_directories(src/include)
build_static_extension(gridpin_ext src/gridpin_extension.cpp)
build_loadable_extension(gridpin_ext " " src/gridpin_extension.cpp)
find_package(Threads REQUIRED)
foreach(GRIDPIN_TARGET gridpin_ext_extension gridpin_ext_loadable_extension)
  add_dependencies(${GRIDPIN_TARGET} gridpin_rust)
  target_compile_definitions(${GRIDPIN_TARGET} PRIVATE GRIDPIN_EXTENSION_VERSION="v${GRIDPIN_VERSION}")
  target_link_libraries(${GRIDPIN_TARGET} "${GRIDPIN_RUST_LIBRARY}" Threads::Threads ${CMAKE_DL_LIBS})
  if(WIN32)
    target_link_libraries(${GRIDPIN_TARGET} ws2_32 userenv bcrypt ntdll advapi32)
  elseif(UNIX AND NOT APPLE)
    target_link_libraries(${GRIDPIN_TARGET} m)
  endif()
endforeach()
install(TARGETS gridpin_ext_extension EXPORT "${DUCKDB_EXPORT_SET}"
  LIBRARY DESTINATION "${INSTALL_LIB_DIR}" ARCHIVE DESTINATION "${INSTALL_LIB_DIR}")
