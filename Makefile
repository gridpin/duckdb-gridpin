PROJ_DIR := $(dir $(abspath $(lastword $(MAKEFILE_LIST))))
EXT_NAME=gridpin_ext
EXT_CONFIG=${PROJ_DIR}extension_config.cmake
TARGET_DUCKDB_VERSION=v1.5.5
include extension-ci-tools/makefiles/duckdb_extension.Makefile
