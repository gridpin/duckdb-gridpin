#define DUCKDB_EXTENSION_MAIN
#include "gridpin_extension.hpp"
#include "duckdb/function/scalar_function.hpp"
#include "duckdb/parser/parsed_data/create_scalar_function_info.hpp"
#include "duckdb/planner/expression/bound_function_expression.hpp"
#include <cstdint>
#include <memory>
#include <string>
#include <type_traits>
#include <utility>
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
// DuckDB 2.0 made the bound function private, result buffers read-only by default and dropped the
// count argument of ToUnifiedFormat; 1.5.x has none of the new accessors, and 2.0 snapshots adopted
// them at different commits. Each accessor is therefore detected on its own at compile time, so one
// source tree builds the catalog `ref` (1.5.x) and `ref_next` (2.0).
template <class E, class = void> struct GridpinHasFunctionAccessor : std::false_type {};
template <class E>
struct GridpinHasFunctionAccessor<E, std::void_t<decltype(std::declval<const E &>().Function())>> : std::true_type {};
template <class F, class = void> struct GridpinHasMutableData : std::false_type {};
template <class F>
struct GridpinHasMutableData<F, std::void_t<decltype(F::template GetDataMutable<string_t>(std::declval<Vector &>()))>>
    : std::true_type {};
template <class F, class = void> struct GridpinHasMutableValidity : std::false_type {};
template <class F>
struct GridpinHasMutableValidity<F, std::void_t<decltype(F::ValidityMutable(std::declval<Vector &>()))>> : std::true_type {};
template <class V, class = void> struct GridpinHasUnifiedWithoutCount : std::false_type {};
template <class V>
struct GridpinHasUnifiedWithoutCount<V, std::void_t<decltype(std::declval<V &>().ToUnifiedFormat(
                                            std::declval<UnifiedVectorFormat &>()))>> : std::true_type {};

template <class E> static inline const auto &GridpinBound(const E &e) {
    if constexpr (GridpinHasFunctionAccessor<E>::value) { return e.Function(); } else { return e.function; }
}
template <class F = FlatVector> static inline string_t *GridpinResultData(Vector &v) {
    if constexpr (GridpinHasMutableData<F>::value) { return F::template GetDataMutable<string_t>(v); }
    else { return F::template GetData<string_t>(v); }
}
template <class F = FlatVector> static inline ValidityMask &GridpinResultValidity(Vector &v) {
    if constexpr (GridpinHasMutableValidity<F>::value) { return F::ValidityMutable(v); } else { return F::Validity(v); }
}
// 2.0 reports parameter names from the function signature (duckdb_functions() showed col0, col1 with only the
// description set); 1.5.x has no signature and takes them from the description, which is still filled below.
template <class S, class = void> struct GridpinHasSignature : std::false_type {};
template <class S>
struct GridpinHasSignature<S, std::void_t<decltype(std::declval<S &>().GetSignature())>> : std::true_type {};
template <class S>
static inline void GridpinNameParameters(S &scalar, const vector<LogicalType> &types, const vector<string> &names) {
    if constexpr (GridpinHasSignature<S>::value) {
        using Signature = std::decay_t<decltype(scalar.GetSignature())>;
        Signature signature(vector<LogicalType> {}, LogicalType::VARCHAR);
        for (idx_t i = 0; i < types.size(); i++) { signature.AddPositionalOnly(names[i].c_str(), types[i]); }
        scalar.GetSignature() = std::move(signature);
    }
}
template <class V> static inline void GridpinUnified(V &v, idx_t count, UnifiedVectorFormat &f) {
    if constexpr (GridpinHasUnifiedWithoutCount<V>::value) { v.ToUnifiedFormat(f); } else { v.ToUnifiedFormat(count, f); }
}

struct GridpinInfo : ScalarFunctionInfo {
    explicit GridpinInfo(std::shared_ptr<GridpinState> state) : state(std::move(state)) {}
    std::shared_ptr<GridpinState> state;
};

template <uint32_t OPERATION>
static void Invoke(DataChunk &args, ExpressionState &state, Vector &result) {
    auto &expression = state.expr.Cast<BoundFunctionExpression>();
    auto &info = GridpinBound(expression).GetExtraFunctionInfo().Cast<GridpinInfo>();
    const auto count = args.size();
    std::vector<GridpinText> texts;
    std::vector<std::string> storage;
    std::vector<double> lats, lons;
    std::vector<uint8_t> nulls;
    if (OPERATION == 4) {
        UnifiedVectorFormat lat_format, lon_format;
        GridpinUnified(args.data[0], count, lat_format);
        GridpinUnified(args.data[1], count, lon_format);
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
        GridpinUnified(args.data[0], count, format);
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
    auto output = GridpinResultData(result);
    auto &validity = GridpinResultValidity(result);
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
        scalar.SetStability(stability);
        // Every function throws InvalidInputException on bad input; 2.0 invalidates the database when a
        // function that is not marked fallible throws (FATAL), 1.5.x only uses the mark for optimization.
        scalar.SetFallible();
        GridpinNameParameters(scalar, types, names);
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
