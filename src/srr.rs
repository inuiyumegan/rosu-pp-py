use std::fs;
use std::sync::{Mutex, OnceLock};

use pyo3::exceptions::PyOSError;
use pyo3::types::PyAnyMethods;
use pyo3::{Bound, Py, PyAny, PyResult, Python};
use rosu_mods::GameModsIntermode;
use rosu_pp::{
    any::DifficultyAttributes,
    mania::{ManiaDifficultyAttributes, ManiaPerformanceAttributes},
    model::mode::GameMode,
};

use crate::{beatmap::PyBeatmap, difficulty::PyDifficulty, mods::PyGameMods};

// Star-Rating-Rebirth's Python source is embedded so the wheel is self-contained.
const ALGORITHM_SRC: &str = include_str!("../srr/algorithm.py");
const PARSER_SRC: &str = include_str!("../srr/osu_file_parser.py");
const OTHER_PARAMS_SRC: &str = include_str!("../srr/other_params.py");

static SRR_READY: OnceLock<()> = OnceLock::new();

/// Write the embedded SRR Python files to a temp directory and put it on
/// `sys.path` so that `import algorithm` works. Runs at most once per process.
fn ensure_srr_ready(py: Python<'_>) -> PyResult<()> {
    if SRR_READY.get().is_some() {
        return Ok(());
    }

    let dir = std::env::temp_dir().join("rosu_pp_py_srr");
    fs::create_dir_all(&dir).map_err(|e| PyOSError::new_err(e.to_string()))?;
    fs::write(dir.join("algorithm.py"), ALGORITHM_SRC)
        .map_err(|e| PyOSError::new_err(e.to_string()))?;
    fs::write(dir.join("osu_file_parser.py"), PARSER_SRC)
        .map_err(|e| PyOSError::new_err(e.to_string()))?;
    fs::write(dir.join("other_params.py"), OTHER_PARAMS_SRC)
        .map_err(|e| PyOSError::new_err(e.to_string()))?;

    let dir_str = dir.to_string_lossy().to_string();
    let sys = py.import("sys")?;
    let path = sys.getattr("path")?;
    let already_present = path
        .call_method1("__contains__", (dir_str.clone(),))?
        .is_truthy()?;
    if !already_present {
        path.call_method1("insert", (0, dir_str))?;
    }

    let _ = SRR_READY.set(());
    Ok(())
}

/// Map the configured mods onto the mod string understood by SRR.
fn srr_mod_string(
    mods: Option<&Py<PyAny>>,
    mode: GameMode,
    py: Python<'_>,
) -> PyResult<&'static str> {
    let gamemods = PyGameMods::extract(mods, mode, py)?;

    let clock_rate = match gamemods {
        PyGameMods::Legacy(ref l) => GameModsIntermode::from(l.clone()).legacy_clock_rate(),
        PyGameMods::Intermode(ref i) => i.legacy_clock_rate(),
        PyGameMods::Lazer(ref l) => GameModsIntermode::from(l.clone()).legacy_clock_rate(),
    };

    Ok(if clock_rate > 1.0 {
        "DT"
    } else if clock_rate < 1.0 {
        "HT"
    } else {
        "NM"
    })
}

/// Check whether NF or EZ mod is active.
fn has_nf_or_ez(
    mods: Option<&Py<PyAny>>,
    mode: GameMode,
    py: Python<'_>,
) -> PyResult<(bool, bool)> {
    let gamemods = PyGameMods::extract(mods, mode, py)?;
    let intermode = match gamemods {
        PyGameMods::Legacy(ref l) => GameModsIntermode::from(l.clone()),
        PyGameMods::Intermode(ref i) => i.clone(),
        PyGameMods::Lazer(ref l) => GameModsIntermode::from(l.clone()),
    };
    Ok((
        intermode.contains(rosu_mods::GameModIntermode::NoFail),
        intermode.contains(rosu_mods::GameModIntermode::Easy),
    ))
}

/// SR params returned by the SRR algorithm.
#[derive(Clone)]
pub(crate) struct SrrParams {
    pub stars: f64,
    pub variety: f64,
    pub acc_scalar: f64,
    pub total_notes: f64,
}

/// Compute mania difficulty attributes using the Star-Rating-Rebirth algorithm.
///
/// Returns `Ok(None)` when SRR cannot be used so the caller can fall back to
/// rosu-pp's built-in mania calculator.
pub(crate) fn mania_difficulty_attrs(
    difficulty: &PyDifficulty,
    map: &PyBeatmap,
    py: Python<'_>,
) -> PyResult<Option<ManiaDifficultyAttributes>> {
    if map.inner.mode != GameMode::Mania || map.inner.is_convert {
        return Ok(None);
    }

    let Some(path) = map.path.as_ref() else {
        return Ok(None);
    };

    ensure_srr_ready(py)?;

    let mod_str = srr_mod_string(difficulty.mods.as_ref(), map.inner.mode, py)?;
    let path_str = path.to_string_lossy().to_string();

    let srr = call_srr(py, &path_str, mod_str)?;

    // Use rosu-pp's own mania calculation for mode-level bookkeeping
    // (n_objects, n_hold_notes, max_combo, is_convert) but replace stars.
    let base = difficulty
        .try_as_difficulty(map.inner.mode, py)?
        .calculate(&map.inner);

    let mut attrs = match base {
        DifficultyAttributes::Mania(a) => a,
        _ => return Ok(None),
    };
    attrs.stars = srr.stars;

    // Store the extra params in a global so the performance calculator can
    // access them. This is a simplification — ideally we'd pass them through
    // the attributes, but ManiaDifficultyAttributes doesn't have these fields.
    set_srr_params(srr);

    Ok(Some(attrs))
}

static SRR_PARAMS: OnceLock<Mutex<Option<SrrParams>>> = OnceLock::new();

fn srr_params_mutex() -> &'static Mutex<Option<SrrParams>> {
    SRR_PARAMS.get_or_init(|| Mutex::new(None))
}

fn set_srr_params(params: SrrParams) {
    *srr_params_mutex().lock().unwrap() = Some(params);
}

pub(crate) fn get_srr_params() -> Option<SrrParams> {
    srr_params_mutex().lock().unwrap().clone()
}

/// Compute mania performance using the sunny-rework PP formula.
pub(crate) fn mania_performance(
    stars: f64,
    n320: u32,
    n300: u32,
    n200: u32,
    n100: u32,
    n50: u32,
    misses: u32,
    mods: Option<&Py<PyAny>>,
    mode: GameMode,
    py: Python<'_>,
) -> PyResult<ManiaPerformanceAttributes> {
    let total_hits = n320 + n300 + n200 + n100 + n50 + misses;

    // Custom accuracy (sunny-rework uses 305, not 320!)
    let custom_acc = if total_hits == 0 {
        0.0
    } else {
        f64::from(n320 * 305 + n300 * 300 + n200 * 200 + n100 * 100 + n50 * 50)
            / f64::from(total_hits * 305)
    };

    // Performance proportion
    let proportion = if custom_acc > 0.80 {
        4.5 * (custom_acc - 0.8)
            / f64::powf(100.0 * (1.0 - custom_acc) + f64::powf(0.9, 20.0), 0.05)
    } else {
        0.0
    };

    // Difficulty value
    let difficulty_value =
        9.8 * f64::powf(f64::max(stars - 0.15, 0.05), 2.2) * proportion;

    // Mod multipliers
    let (has_nf, has_ez) = has_nf_or_ez(mods, mode, py)?;
    let mut multiplier = 1.0;
    if has_nf {
        multiplier *= 0.75;
    }
    if has_ez {
        multiplier *= 0.90;
    }

    // Get SRR params for variety/acc_scalar/total_notes
    let (variety, acc_scalar, total_notes) = match get_srr_params() {
        Some(p) => (p.variety, p.acc_scalar, p.total_notes),
        None => (3.25, 1.0, total_hits as f64), // fallback
    };

    // Variety multiplier (sigmoid)
    let variety_mult = {
        let floor = 0.945;
        let cap = 1.055;
        let l = cap - floor;
        let v0 = 3.25;
        let k = 3.0;
        floor + l / (1.0 + f64::exp(-k * (variety - v0)))
    };

    // Accuracy multiplier
    let acc_mult = {
        let sigmoid_scaler = 0.87 + 0.26 / (1.0 + f64::exp(-20.0 * (acc_scalar - 1.0)));
        sigmoid_scaler * (2.0 * f64::powf(custom_acc, 20.0) - 1.0)
            + 2.0
            - 2.0 * f64::powf(custom_acc, 20.0)
    };

    // Length multiplier
    let length_mult = 1.1 / (1.0 + f64::sqrt(stars / (2.0 * total_notes)));

    let pp = difficulty_value * multiplier * variety_mult * acc_mult * length_mult;

    Ok(ManiaPerformanceAttributes {
        difficulty: ManiaDifficultyAttributes {
            stars,
            n_objects: total_hits,
            n_hold_notes: 0,
            max_combo: 0,
            is_convert: false,
        },
        pp,
        pp_difficulty: difficulty_value,
    })
}

fn call_srr(py: Python<'_>, path: &str, mod_str: &str) -> PyResult<SrrParams> {
    let algorithm: Bound<'_, _> = py.import("algorithm")?;
    let result = algorithm.getattr("calculate")?.call1((path, mod_str))?;

    let stars: f64 = result.get_item("SR")?.extract()?;
    let spikiness: f64 = result.get_item("spikiness")?.extract()?;
    let switches: f64 = result.get_item("switches")?.extract()?;
    let variety: f64 = result.get_item("variety")?.extract()?;
    let total_notes: f64 = result.get_item("total_notes")?.extract()?;

    Ok(SrrParams {
        stars,
        variety,
        acc_scalar: 0.5 * spikiness + 0.5 * switches,
        total_notes,
    })
}
