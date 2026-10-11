#define DUCKDB_EXTENSION_MAIN
#include "gridpin_extension.hpp"
#include "duckdb/function/scalar_function.hpp"
#include "duckdb/parser/parsed_data/create_scalar_function_info.hpp"
#include "duckdb/planner/expression/bound_function_expression.hpp"
#include <cstdint>
#include <memory>
#include <string>
#include <vector>

extern "C" {
struct GridpinState;
struct GridpinResponse;
struct GridpinText {
    const uint8_t *data;
    size_t len;
    uint8_t is_null;
};
GridpinState *gridpin_state_new();
void gridpin_state_drop(GridpinState *);
GridpinResponse *gridpin_invoke(const GridpinState *, uint32_t, const GridpinText *, const double *,
                              const double *, const uint8_t *, size_t);
uint8_t gridpin_response_get(const GridpinResponse *, size_t, GridpinText *);
void gridpin_response_drop(GridpinResponse *);
}

namespace duckdb {
struct GridpinInfo : ScalarFunctionInfo {
    explicit GridpinInfo(std::shared_ptr<GridpinState> state) : state(std::move(state)) {}
    std::shared_ptr<GridpinState> state;
};

template <uint32_t OPERATION>
static void Invoke(DataChunk &args, ExpressionState &state, Vector &result) {
    auto &expression = state.expr.Cast<BoundFunctionExpression>();
    auto &info = expression.function.GetExtraFunctionInfo().Cast<GridpinInfo>();
    const auto count = args.size();
    std::vector<GridpinText> texts;
    std::vector<std::string> storage;
    std::vector<double> lats, lons;
    std::vector<uint8_t> nulls;
    if (OPERATION == 4) {
        UnifiedVectorFormat lat_format, lon_format;
        args.data[0].ToUnifiedFormat(count, lat_format);
        args.data[1].ToUnifiedFormat(count, lon_format);
        auto lat_data = UnifiedVectorFormat::GetData<double>(lat_format);
        auto lon_data = UnifiedVectorFormat::GetData<double>(lon_format);
        lats.resize(count); lons.resize(count); nulls.resize(count);
        for (idx_t i = 0; i < count; i++) {
            auto lat_row = lat_format.sel->get_index(i);
            auto lon_row = lon_format.sel->get_index(i);
            nulls[i] = !lat_format.validity.RowIsValid(lat_row) || !lon_format.validity.RowIsValid(lon_row);
            if (!nulls[i]) { lats[i] = lat_data[lat_row]; lons[i] = lon_data[lon_row]; }
        }
    } else if (OPERATION != 2) {
        UnifiedVectorFormat format;
        args.data[0].ToUnifiedFormat(count, format);
        auto data = UnifiedVectorFormat::GetData<string_t>(format);
        storage.reserve(count); texts.reserve(count);
        for (idx_t i = 0; i < count; i++) {
            auto row = format.sel->get_index(i);
            if (!format.validity.RowIsValid(row)) {
                texts.push_back({nullptr, 0, 1});
            } else {
                storage.push_back(data[row].GetString());
                auto &value = storage.back();
                texts.push_back({reinterpret_cast<const uint8_t *>(value.data()), value.size(), 0});
            }
        }
    }
    std::unique_ptr<GridpinResponse, decltype(&gridpin_response_drop)> response(
        gridpin_invoke(info.state.get(), OPERATION, texts.data(), lats.data(), lons.data(), nulls.data(), count),
        gridpin_response_drop);
    if (!response) { throw InvalidInputException("GridPin: internal error at Rust boundary"); }
    result.SetVectorType(VectorType::FLAT_VECTOR);
    auto output = FlatVector::GetData<string_t>(result);
    auto &validity = FlatVector::Validity(result);
    for (idx_t i = 0; i < count; i++) {
        GridpinText value {};
        auto status = gridpin_response_get(response.get(), i, &value);
        if (status == 1) {
            throw InvalidInputException(std::string(reinterpret_cast<const char *>(value.data), value.len));
        }
        if (status != 0) { throw InvalidInputException("GridPin: invalid Rust response"); }
        if (value.is_null) { validity.SetInvalid(i); }
        else {
            output[i] = StringVector::AddString(result, reinterpret_cast<const char *>(value.data), value.len);
        }
    }
}

static void Register(ExtensionLoader &loader) {
    std::shared_ptr<GridpinState> state(gridpin_state_new(), gridpin_state_drop);
    if (!state) { throw InvalidInputException("GridPin: cannot allocate Rust state"); }
    auto add = [&](const char *name, vector<LogicalType> types, vector<string> names,
                   scalar_function_t function, const char *description, const char *example,
                   FunctionStability stability = FunctionStability::CONSISTENT) {
        ScalarFunction scalar(name, types, LogicalType::VARCHAR, function);
        scalar.stability = stability;
        scalar.SetExtraFunctionInfo<GridpinInfo>(state);
        CreateScalarFunctionInfo info(scalar);
        info.on_conflict = OnCreateConflict::ALTER_ON_CONFLICT;
        FunctionDescription details;
        details.parameter_types = std::move(types);
        details.parameter_names = std::move(names);
        details.description = description;
        details.examples = {example};
        details.categories = {"geocoding"};
        info.descriptions.push_back(std::move(details));
        loader.RegisterFunction(std::move(info));
    };
    add("gridpin_load", {LogicalType::VARCHAR}, {"path"}, Invoke<0>,
        "Load an address index for this database, requiring an explicit reset before changing its path.",
        "SELECT gridpin_load('mc.bin')");
    add("gridpin_load_poi", {LogicalType::VARCHAR}, {"path"}, Invoke<1>,
        "Load an optional POI index that must match the address index country.",
        "SELECT gridpin_load_poi('mc_poi.bin')");
    add("gridpin_reset", {}, {}, Invoke<2>,
        "Unload the address and POI indexes in a standalone statement before switching sheets.",
        "SELECT gridpin_reset()", FunctionStability::VOLATILE);
    add("gridpin_geocode", {LogicalType::VARCHAR}, {"query"}, Invoke<3>,
        "Return the best address or POI match as JSON, or an empty JSON object when nothing matches.",
        "SELECT gridpin_geocode('boulevard des moulins 27 monaco')");
    add("gridpin_reverse", {LogicalType::DOUBLE, LogicalType::DOUBLE}, {"lat", "lon"}, Invoke<4>,
        "Return the nearest indexed house as JSON for finite coordinates, or an empty JSON object when none is nearby.",
        "SELECT gridpin_reverse(43.7391, 7.4266)");
}

void GridpinExtension::Load(ExtensionLoader &loader) { Register(loader); }
} // namespace duckdb

extern "C" {
DUCKDB_CPP_EXTENSION_ENTRY(gridpin_ext, loader) { duckdb::Register(loader); }
}
