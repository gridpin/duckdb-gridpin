# duckdb-gridpin

The [GridPin](https://gridpin.dev) DuckDB extension: offline forward and reverse geocoding
against a single memory-mapped country file. No server, no network, no per-request fees.

```sql
LOAD gridpin_ext;
SELECT gridpin_load('./france.bin');
SELECT gridpin_geocode('10 rue de rivoli paris');
SELECT gridpin_reverse(48.8686, 2.3305);
```

Country data files are published as free downloads — see
[gridpin.dev](https://gridpin.dev) and the [releases of the main
repository](https://github.com/gridpin/gridpin/releases).

## What this repository is

This repository holds **only the extension crate**, laid out the way
[extension-template-rs](https://github.com/duckdb/extension-template-rs) expects, because the
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

Apache-2.0. Data files carry their own licences, listed per country sheet.
