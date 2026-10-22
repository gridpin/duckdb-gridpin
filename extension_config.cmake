file(STRINGS "${CMAKE_CURRENT_LIST_DIR}/Cargo.toml" GRIDPIN_VERSION_LINE REGEX "^version =")
string(REGEX REPLACE "^version = \"([^\"]+)\".*" "\\1" GRIDPIN_VERSION "${GRIDPIN_VERSION_LINE}")
# The extension is linked into the DuckDB binaries the build makes, unittest included: sqllogictest's
# `require gridpin_ext` loads a linked extension, and silently skips the whole test file otherwise
# ("Extensions built but not linked", mirror run 39611788181). The loadable file is built either way.
# DuckDB 1.5.x links every extension it loads; 2.0 removed DONT_LINK and links only the ones listed with
# duckdb_extension_statically_link(), which exists only from 2.0 on.
duckdb_extension_load(gridpin_ext
  SOURCE_DIR "${CMAKE_CURRENT_LIST_DIR}"
  EXTENSION_VERSION "v${GRIDPIN_VERSION}")
if(COMMAND duckdb_extension_statically_link)
  duckdb_extension_statically_link(gridpin_ext)
endif()
