#pragma once
#include "duckdb.hpp"
#include "duckdb/main/extension/extension_loader.hpp"

namespace duckdb {
class GridpinExtension : public Extension {
public:
    void Load(ExtensionLoader &loader) override;
    std::string Name() override { return "gridpin_ext"; }
    std::string Version() const override { return GRIDPIN_EXTENSION_VERSION; }
};
}
