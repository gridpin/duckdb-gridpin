"""Runnable tests for the gridpin DuckDB extension.

Builds nothing itself — point GRIDPIN_EXT at the built .duckdb_extension (default:
duckdb-ext/gridpin_ext.duckdb_extension) and GRIDPIN_TEST_SHEET at a .bin (default:
data/mc.bin, the Monaco smoke sheet). Run:  pytest duckdb-ext/test/
"""
import json
import os
import pathlib

import pytest

duckdb = pytest.importorskip("duckdb")

CODE = pathlib.Path(__file__).resolve().parents[2]
EXT = pathlib.Path(os.environ.get("GRIDPIN_EXT", CODE / "duckdb-ext" / "gridpin_ext.duckdb_extension"))
SHEET = pathlib.Path(os.environ.get("GRIDPIN_TEST_SHEET", CODE / "data" / "mc.bin"))


@pytest.fixture()
def con():
    if not EXT.exists():
        pytest.skip(f"no extension at {EXT} (build it: make bindings)")
    if not SHEET.exists():
        pytest.skip(f"no test sheet at {SHEET}")
    c = duckdb.connect(config={"allow_unsigned_extensions": "true"})
    c.execute(f"LOAD '{EXT}'")
    return c



@pytest.fixture(scope="session")
def poi_sheet(tmp_path_factory):
    """A tiny real POI sheet (country=mc, layer=poi) so the cascade/reset can be
    exercised for real instead of self-referentially. Built with the gridpin CLI."""
    cli = CODE / "gridpin" / "target" / "release" / "gridpin"
    if not cli.exists():
        pytest.skip("gridpin CLI not built (cargo build --release)")
    d = tmp_path_factory.mktemp("poi")
    csv = d / "poi.csv"
    csv.write_text(
        "nom_voie_norm,code_insee,nom_commune_norm,code_postal,numero,rep,lon,lat,nom_voie,nom_commune\n"
        "cafe gridpin,001,monaco,98000,1,,7.4270,43.7395,Cafe Gridpin,Monaco\n"
    )
    man = d / "poi.json"
    man.write_text('{"country": "mc", "layer": "poi", "license": "test", "source_release": "test"}')
    out = d / "mc_poi.bin"
    import subprocess
    subprocess.run([str(cli), "build", str(csv), str(out), "--meta", str(man)], check=True,
                   capture_output=True)
    return out


def test_four_functions_exist_and_load(con):
    assert "index loaded" in con.execute(f"SELECT gridpin_load('{SHEET}')").fetchone()[0]
    r = con.execute("SELECT gridpin_geocode('boulevard des moulins 27 monaco')").fetchone()[0]
    assert json.loads(r).get("precision") in ("house", "near", "street", "city")
    rev = con.execute("SELECT gridpin_reverse(43.7391, 7.4266)").fetchone()[0]
    assert isinstance(json.loads(rev), dict)


def test_empty_result_contract_is_braces(con):
    con.execute(f"SELECT gridpin_load('{SHEET}')")
    # documented contract: no match -> '{}' (not NULL, not an error)
    assert con.execute("SELECT gridpin_geocode('xyzzy nonexistent 999')").fetchone()[0] == "{}"


def test_geocode_before_load_errors_clearly(con):
    with pytest.raises(Exception) as e:
        con.execute("SELECT gridpin_geocode('test')").fetchone()
    assert "gridpin_load" in str(e.value)


def test_corrupt_sheet_errors_not_crashes(con, tmp_path):
    bad = tmp_path / "bad.bin"
    bad.write_bytes(b"garbage not a sheet")
    with pytest.raises(Exception):
        con.execute(f"SELECT gridpin_load('{bad}')").fetchone()
    # the connection is still alive after the error
    assert con.execute("SELECT 1").fetchone()[0] == 1


def test_reverse_non_finite_errors_not_crashes(con):
    # M-02 at the DuckDB boundary: NaN/Inf/out-of-range coordinates raise a clear error (via the
    # strict try_reverse), not a crash and not a silent '{}'. A NULL coordinate, by SQL semantics,
    # yields NULL — not an error. The connection must survive both.
    con.execute(f"SELECT gridpin_load('{SHEET}')")
    for lat, lon in [("'nan'::DOUBLE", "7.42"), ("43.7", "'inf'::DOUBLE"),
                     ("91.0", "7.42"), ("43.7", "200.0")]:
        with pytest.raises(Exception):
            con.execute(f"SELECT gridpin_reverse({lat}, {lon})").fetchone()
    # NULL is not an error — it is NULL (SQL semantics), and a valid point still resolves
    assert con.execute("SELECT gridpin_reverse(NULL, 7.42)").fetchone()[0] is None
    assert isinstance(json.loads(con.execute("SELECT gridpin_reverse(43.7391, 7.4266)").fetchone()[0]), dict)
    assert con.execute("SELECT 1").fetchone()[0] == 1, "connection alive after the errors"


def test_null_path_in_a_column_yields_null_not_error(con):
    # SQL semantics (audit-16 M-02): a NULL row must not kill the whole query.
    # And only the LAST non-NULL path is opened (audit-16 H-01): per-row opens
    # would leak one mmap per row on a column argument.
    rows = con.execute(
        f"SELECT s, gridpin_load(s) FROM (VALUES (NULL), ('{SHEET}')) t(s) ORDER BY s NULLS FIRST"
    ).fetchall()
    assert rows[0][1] is None
    assert "index loaded" in rows[1][1]
    # the index did load: a query works
    r = con.execute("SELECT gridpin_geocode('boulevard des moulins 27 monaco')").fetchone()[0]
    assert json.loads(r)


def test_multi_chunk_constant_path_loads_once_and_answers(con):
    # audit R7-H14: a load argument that spans MULTIPLE DataChunks (>2048 rows) but is the SAME
    # constant path everywhere must load once (idempotent, no per-chunk reopen/leak) and answer.
    con.execute(
        # CASE keeps it a non-constant expression (a real column, multiple chunks), always == SHEET
        f"SELECT gridpin_load(CASE WHEN i >= 0 THEN '{SHEET}' END) FROM range(5000) t(i)"
    ).fetchall()
    r = con.execute("SELECT gridpin_geocode('boulevard des moulins 27 monaco')").fetchone()[0]
    assert json.loads(r), "the sheet loaded across chunks and answers"


def test_differing_paths_in_one_call_is_a_hard_error(con):
    # audit R7-H14: the load argument must be a single CONSTANT path — a column of DIFFERING paths
    # must be a clear error, not a silent "last one in the chunk wins".
    with pytest.raises(duckdb.Error) as e:
        con.execute(
            f"SELECT gridpin_load(CASE WHEN i = 0 THEN '{SHEET}' ELSE '{SHEET}.other' END) "
            "FROM range(10) t(i)"
        ).fetchall()
    assert "single constant path" in str(e.value)


def test_poi_loaded_first_survives_loading_the_same_country_address(con, poi_sheet):
    # self-review round 2: gridpin_load_poi BEFORE gridpin_load used to silently drop the
    # POI layer even for the SAME country. Now gridpin_load keeps a POI that still pairs
    # with the new address index; only a country switch drops it.
    con.execute(f"SELECT gridpin_load_poi('{poi_sheet}')")   # POI first
    con.execute(f"SELECT gridpin_load('{SHEET}')")           # then the same-country address sheet
    hit = json.loads(con.execute("SELECT gridpin_geocode('cafe gridpin monaco')").fetchone()[0])
    assert "poi_layer" in (hit.get("flags") or []), f"POI loaded first must survive: {hit}"


@pytest.fixture(scope="session")
def other_country_sheet(tmp_path_factory):
    """A tiny address sheet for a DIFFERENT country (de), to trigger the POI reset on a
    genuine country switch (self-review round 2)."""
    cli = CODE / "gridpin" / "target" / "release" / "gridpin"
    if not cli.exists():
        pytest.skip("gridpin CLI not built")
    d = tmp_path_factory.mktemp("de")
    csv = d / "de.csv"
    csv.write_text(
        "nom_voie_norm,code_insee,nom_commune_norm,code_postal,numero,rep,lon,lat,nom_voie,nom_commune\n"
        "hauptstrasse,001,berlin,10115,1,,13.40,52.52,Hauptstrasse,Berlin\n"
    )
    man = d / "de.json"; man.write_text('{"country": "de", "layer": "addresses", "license": "test", "source_release": "test"}')
    out = d / "de.bin"
    import subprocess
    subprocess.run([str(cli), "build", str(csv), str(out), "--meta", str(man)], check=True, capture_output=True)
    return out


def test_load_poi_refuses_an_address_sheet(con, other_country_sheet):
    # audit R7-H11: gridpin_load_poi used the permissive open, so an ADDRESS sheet loaded as a POI
    # layer was accepted. It must now be refused (Index::open_poi), symmetric to gridpin_load.
    con.execute(f"SELECT gridpin_load('{SHEET}')")  # a real address index is loaded first
    with pytest.raises(duckdb.Error) as e:
        con.execute(f"SELECT gridpin_load_poi('{other_country_sheet}')").fetchone()
    assert "POI layer" in str(e.value), f"an address sheet as POI must be refused: {e.value}"


def test_poi_cascade_answers_then_reset_load_switches_country(con, poi_sheet, other_country_sheet):
    # Real cascade + reset: load the address sheet and a POI layer, ask for a name only
    # the POI layer knows -> the answer must carry the poi_layer flag. Switching countries is
    # EXPLICIT (audit R8-H14): a direct load of a different path errors; gridpin_reset() +
    # load performs the switch and drops the POI layer with it.
    con.execute(f"SELECT gridpin_load('{SHEET}')")
    con.execute(f"SELECT gridpin_load_poi('{poi_sheet}')")
    with_poi = json.loads(con.execute("SELECT gridpin_geocode('cafe gridpin monaco')").fetchone()[0])
    assert "poi_layer" in (with_poi.get("flags") or []), f"POI should answer: {with_poi}"
    # a direct switch (no reset) is a hard error pointing at gridpin_reset
    with pytest.raises(duckdb.Error) as e:
        con.execute(f"SELECT gridpin_load('{other_country_sheet}')").fetchone()
    assert "gridpin_reset" in str(e.value), f"switch without reset must be refused: {e.value}"
    # explicit switch: reset unloads BOTH layers, then the new country loads
    assert con.execute("SELECT gridpin_reset()").fetchone()[0] == "index unloaded"
    con.execute(f"SELECT gridpin_load('{other_country_sheet}')")
    after = json.loads(con.execute("SELECT gridpin_geocode('cafe gridpin monaco')").fetchone()[0])
    assert "poi_layer" not in (after.get("flags") or []), "reset must drop the POI layer too"


def test_switching_sheets_requires_an_explicit_reset(con, tmp_path):
    # Runnable proof of the public docs.html FAQ "Can I load two countries at once?" and the
    # DuckDB "switching sheets" example: a repeated gridpin_load with a DIFFERENT path does NOT
    # replace the index — it is a hard error naming gridpin_reset(); the explicit standalone
    # reset then performs the switch. (audit C-02: the FAQ used to claim silent replacement.)
    # The guard is on the path STRING, so a second distinct path (here a symlink to the same valid
    # sheet) is refused just as a genuinely different country would be — and this keeps the test
    # runnable from one small sheet regardless of the extension/sheet build.
    other = tmp_path / "other.bin"
    other.symlink_to(SHEET)
    assert "index loaded" in con.execute(f"SELECT gridpin_load('{SHEET}')").fetchone()[0]
    # loading the SAME path again is a no-op, not an error
    assert "index loaded" in con.execute(f"SELECT gridpin_load('{SHEET}')").fetchone()[0]
    # a DIFFERENT path is refused and the message points at gridpin_reset()
    with pytest.raises(duckdb.Error) as e:
        con.execute(f"SELECT gridpin_load('{other}')").fetchone()
    assert "gridpin_reset" in str(e.value), f"switch without reset must be refused: {e.value}"
    # the refused load changed nothing: the original sheet still answers
    assert con.execute("SELECT gridpin_geocode('boulevard des moulins 27 monaco')").fetchone()[0] != "{}"
    # explicit standalone reset unloads, then the new path loads and answers
    assert con.execute("SELECT gridpin_reset()").fetchone()[0] == "index unloaded"
    assert "index loaded" in con.execute(f"SELECT gridpin_load('{other}')").fetchone()[0]
    assert con.execute("SELECT gridpin_geocode('boulevard des moulins 27 monaco')").fetchone()[0] != "{}"


def test_path_flip_between_chunks_is_a_hard_error(con, other_country_sheet):
    # R8-H14 repro: a >2048-row path column whose value FLIPS between DataChunks of
    # ONE query used to silently swap the loaded index mid-query (each chunk was internally
    # constant, so the per-chunk check passed). Flip exactly at the 2048 chunk boundary: chunk 0 is
    # all mc, chunk 1 all de -> the second chunk must be a deterministic error, never a swap.
    with pytest.raises(duckdb.Error) as e:
        con.execute(
            f"SELECT gridpin_load(CASE WHEN i < 2048 THEN '{SHEET}' ELSE '{other_country_sheet}' END) "
            "FROM range(5000) t(i)"
        ).fetchall()
    assert "gridpin_reset" in str(e.value), f"cross-chunk flip must be refused: {e.value}"
    # the first chunk's sheet stayed loaded — the state was not corrupted by the refused flip
    r = con.execute("SELECT gridpin_geocode('boulevard des moulins 27 monaco')").fetchone()[0]
    assert json.loads(r), "the originally loaded sheet still answers"


def test_trailing_single_row_chunk_cannot_swap_the_index(con, other_country_sheet):
    # R8-H14 airtight case: the flip lands in a trailing 1-ROW chunk (4097 rows: 2048+2048+1).
    # A row-count heuristic would let this one through; the no-implicit-switch rule refuses it too.
    with pytest.raises(duckdb.Error) as e:
        con.execute(
            f"SELECT gridpin_load(CASE WHEN i < 4096 THEN '{SHEET}' ELSE '{other_country_sheet}' END) "
            "FROM range(4097) t(i)"
        ).fetchall()
    assert "gridpin_reset" in str(e.value), f"a 1-row trailing chunk must not swap: {e.value}"


def test_reset_inside_a_multirow_query_cannot_rearm_a_swap(con, other_country_sheet):
    # audit R8-H14 round 2: SELECT gridpin_reset(), gridpin_load(<flip column>), geocode(...)
    # re-armed the load every chunk and mixed two countries in one result set. gridpin_reset now
    # refuses any multi-row invocation, so the whole statement errors at the first chunk.
    con.execute(f"SELECT gridpin_load('{SHEET}')")
    with pytest.raises(duckdb.Error) as e:
        con.execute(
            "SELECT gridpin_reset(), "
            f"gridpin_load(CASE WHEN i < 2048 THEN '{SHEET}' ELSE '{other_country_sheet}' END), "
            "gridpin_geocode('hauptstrasse 1 berlin') FROM range(4096) t(i)"
        ).fetchall()
    assert "standalone" in str(e.value), f"multi-row reset must be refused: {e.value}"
    # the originally loaded sheet is intact and the legit single-row reset still works
    assert json.loads(con.execute("SELECT gridpin_geocode('boulevard des moulins 27 monaco')").fetchone()[0])
    assert con.execute("SELECT gridpin_reset()").fetchone()[0] == "index unloaded"
