//! The table of everything we implement.

use std::collections::HashMap;
use std::sync::LazyLock;

use crate::modules;

/// One function we implement, under the name the title imports (`#n` for an
/// ordinal).
#[derive(Debug, Clone, Copy)]
pub struct HleExport {
    pub name: &'static str,
    pub address: usize,
}

/// A group of implementations and the DLL names they answer for.
#[derive(Debug)]
pub struct HleModule {
    /// Human-readable name, e.g. "kernel".
    pub name: &'static str,
    /// Lower-case DLL names served. A trailing `*` matches a prefix, which
    /// covers versioned API sets (`api-ms-win-core-libraryloader-l1-2-0.dll`).
    pub dlls: &'static [&'static str],
    pub exports: Vec<HleExport>,
}

impl HleModule {
    pub fn serves(&self, dll: &str) -> bool {
        self.dlls.iter().any(|pattern| dll_matches(pattern, dll))
    }
}

/// `dll` (any case) against a lower-case pattern whose trailing `*` matches
/// a prefix; no allocation (import binding and `GetProcAddress` ask often).
pub(crate) fn dll_matches(pattern: &str, dll: &str) -> bool {
    let (pattern, dll) = (pattern.as_bytes(), dll.as_bytes());
    match pattern.strip_suffix(b"*") {
        Some(prefix) => {
            dll.len() >= prefix.len() && dll[..prefix.len()].eq_ignore_ascii_case(prefix)
        }
        None => dll.eq_ignore_ascii_case(pattern),
    }
}

/// Declares an export whose Rust name equals the imported name.
#[macro_export]
macro_rules! hle_export {
    ($f:ident) => {
        $crate::HleExport {
            name: stringify!($f),
            address: $f as *const () as usize,
        }
    };
}

static REGISTRY: LazyLock<Vec<HleModule>> = LazyLock::new(modules::all);

/// All modules we implement.
pub fn registry() -> &'static [HleModule] {
    &REGISTRY
}

/// Each export name with the modules that have it, in registry order.
static BY_NAME: LazyLock<HashMap<&'static str, Vec<(&'static HleModule, usize)>>> =
    LazyLock::new(|| {
        let mut by_name: HashMap<_, Vec<_>> = HashMap::new();
        for m in registry() {
            for e in &m.exports {
                by_name.entry(e.name).or_default().push((m, e.address));
            }
        }
        by_name
    });

/// The first module (in registry order) that serves `dll` and has `name`.
pub(crate) fn lookup(dll: &str, name: &str) -> Option<usize> {
    BY_NAME
        .get(name)?
        .iter()
        .find(|(m, _)| m.serves(dll))
        .map(|&(_, address)| address)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_names_are_unique_per_dll_pattern() {
        for module in registry() {
            let mut names: Vec<_> = module.exports.iter().map(|e| e.name).collect();
            names.sort_unstable();
            let before = names.len();
            names.dedup();
            assert_eq!(
                before,
                names.len(),
                "duplicate export in module {}",
                module.name
            );
        }
    }

    /// The index answers as the scan of every module did.
    #[test]
    fn lookups_find_the_first_module_that_serves_the_dll() {
        let scan = |dll: &str, name: &str| {
            registry()
                .iter()
                .filter(|m| m.serves(dll))
                .flat_map(|m| m.exports.iter())
                .find(|e| e.name == name)
                .map(|e| e.address)
        };
        let dlls = [
            "kernel32.dll",
            "KERNELBASE.dll",
            "api-ms-win-core-file-l1-1-0.dll",
            "api-ms-win-core-processthreads-l1-1-1.dll",
            "xgameruntime.dll",
            "nothing.dll",
        ];
        for m in registry() {
            for e in &m.exports {
                for dll in dlls
                    .iter()
                    .copied()
                    .chain(m.dlls.iter().map(|d| d.trim_end_matches('*')))
                {
                    assert_eq!(lookup(dll, e.name), scan(dll, e.name), "{dll}!{}", e.name);
                }
            }
        }
        assert_eq!(lookup("kernel32.dll", "NoSuchExport"), None);
        assert!(dll_matches("kernel32.dll", "KERNEL32.DLL"));
        assert!(!dll_matches("kernel32.dll", "kernel32.dl"));
        assert!(!dll_matches("api-ms-win-core-*", "api-ms-win-cor"));
    }

    #[test]
    fn api_set_prefix_matching() {
        let m = HleModule {
            name: "t",
            dlls: &["api-ms-win-core-libraryloader-*"],
            exports: vec![],
        };
        assert!(m.serves("API-MS-WIN-CORE-LIBRARYLOADER-L1-2-0.dll"));
        assert!(!m.serves("api-ms-win-core-file-l1-1-0.dll"));
    }
}
