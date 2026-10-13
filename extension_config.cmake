file(STRINGS "${CMAKE_CURRENT_LIST_DIR}/Cargo.toml" GRIDPIN_VERSION_LINE REGEX "^version =")
string(REGEX REPLACE "^version = \"([^\"]+)\".*" "\\1" GRIDPIN_VERSION "${GRIDPIN_VERSION_LINE}")
# DuckDB 2.0 removed DONT_LINK: extensions are linked only when listed with
# duckdb_extension_statically_link(), which exists only from 2.0 on. 1.5.x still needs DONT_LINK.
if(COMMAND duckdb_extension_statically_link)
  duckdb_extension_load(gridpin_ext
    SOURCE_DIR "${CMAKE_CURRENT_LIST_DIR}"
    EXTENSION_VERSION "v${GRIDPIN_VERSION}")
else()
  duckdb_extension_load(gridpin_ext
    SOURCE_DIR "${CMAKE_CURRENT_LIST_DIR}"
    DONT_LINK
    EXTENSION_VERSION "v${GRIDPIN_VERSION}")
endif()
