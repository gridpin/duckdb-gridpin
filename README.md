# duckdb-gridpin

The [GridPin](https://gridpin.dev) DuckDB extension: offline forward and reverse geocoding
against a single memory-mapped country file. No server, no network, no per-request fees.

```sql
INSTALL gridpin_ext FROM community;
LOAD gridpin_ext;
SELECT gridpin_load('./france.bin');
SELECT gridpin_geocode('10 rue de rivoli paris');
SELECT gridpin_reverse(48.8686, 2.3305);
```

The community catalog serves the current release on DuckDB 1.5.6 or newer; older DuckDB
versions install an older build. Check what you got with
`SELECT extension_version FROM duckdb_extensions() WHERE extension_name = 'gridpin_ext';`.

Country data files are published as free downloads — see
[gridpin.dev](https://gridpin.dev) and the [releases of the main
repository](https://github.com/gridpin/gridpin/releases).

## What this repository is

This repository holds **the C++ registration layer and Rust static library**, laid out the way
[extension-template](https://github.com/duckdb/extension-template) expects, because the
DuckDB community-extensions catalogue builds an extension from the root of its repository.

It is generated from the GridPin monorepo and is not edited by hand. The geocoding engine
itself lives in [gridpin/gridpin](https://github.com/gridpin/gridpin) and is pulled in as a
git dependency pinned to an exact commit, so a build here is reproducible.

Issues and pull requests are welcome in the [main
repository](https://github.com/gridpin/gridpin).

## Building

```sh
git clone --recurse-submodules https://github.com/gridpin/duckdb-gridpin
cd duckdb-gridpin
make configure
make release
```

## License

Apache-2.0, see [LICENSE](LICENSE). The extension statically links the Rust libraries listed
in [THIRDPARTY.yml](THIRDPARTY.yml), which carries their license texts. Data files carry their
own licences, listed per country sheet.
