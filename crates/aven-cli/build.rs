//! Bake with the real host schema and compiler graph wiring. This lives above
//! aven-host so build-time checking can use its standard globals without
//! duplicating host declarations or making aven-host depend on itself.

use std::error::Error;
use std::fs;
use std::path::PathBuf;
use std::time::Instant;

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=../aven-host/std");
    // Cargo tracks build-dependency code; explicit std tracking also covers the
    // source bytes used by the already-compiled host library.
    let directory = PathBuf::from(std::env::var_os("OUT_DIR").ok_or("missing OUT_DIR")?);
    let entry = directory.join("bake.av");
    let library = aven_host::standard_std_library();
    let mut specifiers = library.keys().collect::<Vec<_>>();
    specifiers.sort();
    let source = specifiers
        .iter()
        .enumerate()
        .map(|(index, specifier)| format!("m{index} = import({specifier:?})\n"))
        .collect::<String>();
    fs::write(&entry, source)?;
    let roots = aven_compiler::ModuleRoots::none()
        .with_library(aven_host::STD_LIBRARY_NAME, library)
        .with_trusted_ambient_modules(aven_host::STD_AMBIENT_METHOD_MODULES.iter().copied())
        .with_trusted_prelude_modules(aven_host::STD_PRELUDE_MODULES.iter().copied())
        .with_library_only_global_names(aven_host::standard_library_only_global_names());
    let start = Instant::now();
    let baked = aven_compiler::bake_library_checks(
        &entry,
        &aven_host::standard_check_host_globals(),
        &roots,
    )?;
    assert!(
        !baked.output.reports.iter().any(|report| report
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.is_error())),
        "embedded std must check at build time: {:?}",
        baked.output.reports
    );
    let check_duration = start.elapsed();
    let mut generated = String::from("&[\n");
    for (index, (specifier, checked)) in baked.modules.iter().enumerate() {
        let file = directory.join(format!("std-{index}.json"));
        fs::write(&file, serde_json::to_vec(checked)?)?;
        generated.push_str(&format!("({specifier:?}, include_bytes!({:?})),\n", file));
    }
    generated.push_str("]\n");
    fs::write(directory.join("baked_std.rs"), generated)?;
    fs::write(
        directory.join("baked-timing.txt"),
        format!(
            "baked {} std modules: graph checks {:?}, total {:?}\n",
            baked.modules.len(),
            check_duration,
            start.elapsed()
        ),
    )?;
    Ok(())
}
