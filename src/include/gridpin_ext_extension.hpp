#pragma once
#include "duckdb.hpp"
#include "duckdb/main/extension/extension_loader.hpp"

namespace duckdb {
// Named after the extension (gridpin_ext -> gridpin_ext_extension.hpp, GridpinExtExtension): DuckDB 1.5's loader for
// statically linked extensions includes "<name>_extension.hpp" and calls LoadStaticExtension<CamelCaseExtension>().
class GridpinExtExtension : public Extension {
public:
    void Load(ExtensionLoader &loader) override;
    std::string Name() override { return "gridpin_ext"; }
    // Defined in gridpin_extension.cpp: the generated loader includes this header outside our targets, where
    // GRIDPIN_EXTENSION_VERSION is not defined.
    std::string Version() const override;
};
}
