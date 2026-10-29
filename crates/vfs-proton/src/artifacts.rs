//! The Windows build artefacts a Proton launch and its tests need, by file
//! name.
//!
//! They are a Windows cross-build (`bin/build-windows`), not part of any
//! Linux build, so every piece of code that has to find them used to carry its
//! own list: the launch, the test support, Haskill's locator and the script
//! itself. This is the one list. The script cannot read Rust, so a test here
//! reads the script's `ARTIFACTS=(…)` array and asserts it equals
//! [`WINDOWS_ARTIFACTS`].

/// The injector: the Windows program Wine runs to start the image and load
/// the shim into it.
pub const INJECTOR: &str = "vfs-injector.exe";
/// The shim DLL, which remaps the program's I/O to the director.
pub const SHIM_DLL: &str = "vfs_shim_dll.dll";
/// What a Proton launch itself needs: the injector and the shim.
pub const LAUNCH: [&str; 2] = [INJECTOR, SHIM_DLL];
/// The i686 injector and shim, for a 32-bit (WOW64) program (the director picks them by the
/// image's bitness: aether.director.natives).
pub const INJECTOR32: &str = "vfs-injector32.exe";
pub const SHIM_DLL32: &str = "vfs_shim_dll32.dll";

/// The Windows test programs the Proton end-to-end tests run.
pub const FIXTURE_READ: &str = "vfs-fixture-read.exe";
pub const FIXTURE_STEAM: &str = "vfs-fixture-steam.exe";
pub const FIXTURE_NVAPI: &str = "vfs-fixture-nvapi.exe";
pub const FIXTURE_REGISTRY: &str = "vfs-fixture-registry.exe";
/// The rest of the fixtures, which the director's end-to-end tests (Cairn's
/// `aether.director.e2e`) run as well: the same set as
/// `bin/build-natives.ps1 -Fixtures` builds on Windows.
pub const FIXTURE_WRITEPATH: &str = "vfs-fixture-writepath.exe";
pub const FIXTURE_ESCAPE: &str = "vfs-fixture-escape.exe";
pub const FIXTURE_PREFS: &str = "vfs-fixture-prefs.exe";
/// vfs-fixture-vproxy's DLL, which `FIXTURE_STATICIMP` imports statically.
pub const FIXTURE_VPROXY: &str = "vproxy.dll";
pub const FIXTURE_STATICIMP: &str = "vfs-staticimp.exe";
/// vfs-inject's probe program.
pub const PROBE: &str = "vfs-probe.exe";

/// Every artefact `bin/build-windows` produces, in the script's order: the
/// launch's two, their 32-bit builds, then the fixtures.
pub const WINDOWS_ARTIFACTS: [&str; 14] = [
    INJECTOR,
    SHIM_DLL,
    INJECTOR32,
    SHIM_DLL32,
    FIXTURE_READ,
    FIXTURE_STEAM,
    FIXTURE_NVAPI,
    FIXTURE_REGISTRY,
    FIXTURE_WRITEPATH,
    FIXTURE_ESCAPE,
    FIXTURE_PREFS,
    FIXTURE_VPROXY,
    FIXTURE_STATICIMP,
    PROBE,
];

#[cfg(test)]
mod tests {
    use super::*;

    /// The names in `bin/build-windows`'s `ARTIFACTS=( … )` array.
    fn script_list() -> Vec<String> {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../bin/build-windows");
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        text.lines()
            .skip_while(|l| l.trim() != "ARTIFACTS=(")
            .skip(1)
            .take_while(|l| l.trim() != ")")
            .map(|l| l.trim().to_string())
            .collect()
    }

    #[test]
    fn the_build_script_builds_exactly_the_artifacts_listed_here() {
        assert_eq!(
            script_list(),
            WINDOWS_ARTIFACTS,
            "bin/build-windows ARTIFACTS"
        );
    }

    #[test]
    fn the_launch_artifacts_are_the_first_of_the_list() {
        assert_eq!(WINDOWS_ARTIFACTS[..LAUNCH.len()], LAUNCH);
    }
}
