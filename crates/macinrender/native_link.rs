//! Audit the libraries `mradm-islands-sys` links: ADM Core's C/C++ islands and
//! codecs must reach the player as static archives, never as a shared library
//! or an import library that would need a DLL beside the executable.
use std::fs;
use std::path::{Path, PathBuf};

/// Every library file the islands' link manifest names, in link order. Its
/// lines are `kind:value`: `file:` an archive, `link:` a codec target that a
/// `target:Name=FILE;LINKS` line resolves to its file and link interface, and
/// `framework:` / `system:` platform libraries the toolchain provides.
fn libraries(manifest: &str) -> Vec<PathBuf> {
    let mut result = Vec::new();
    for line in manifest.lines().filter(|line| !line.trim().is_empty()) {
        let (kind, value) = line
            .split_once(':')
            .unwrap_or_else(|| panic!("Not a link manifest line: {line}"));
        match kind {
            "file" => result.push(PathBuf::from(value)),
            "target" => {
                let (_, spec) = value.split_once('=').expect("target:Name=FILE;LINKS");
                result.extend(
                    spec.split(';')
                        .filter(|item| Path::new(item).is_absolute())
                        .map(PathBuf::from),
                );
            }
            "link" | "framework" | "system" => {}
            _ => panic!("Unexpected native link manifest kind: {kind}"),
        }
    }
    result
}

fn is_archive(path: &Path, windows: bool) -> bool {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    extension == "a" || (windows && extension.eq_ignore_ascii_case("lib"))
}

pub fn audit(manifest: &Path, windows: bool) {
    let text = fs::read_to_string(manifest)
        .unwrap_or_else(|error| panic!("Cannot read {}: {error}", manifest.display()));
    let libraries = libraries(&text);
    assert!(!libraries.is_empty(), "No native static libraries");
    let mut checked = std::collections::HashSet::new();
    for path in libraries {
        assert!(
            is_archive(&path, windows),
            "Unexpected native link input (only static archives and system libraries are supported): {}",
            path.display()
        );
        if checked.insert(path.clone()) {
            verify_archive(&path);
        }
    }
}

fn coff_import_dll(member: &[u8]) -> Option<&str> {
    // IMPORT_OBJECT_HEADER is 20 bytes, followed by NUL-terminated symbol
    // and DLL names. SizeOfData excludes the header and archive padding.
    let size = u32::from_le_bytes(member.get(12..16)?.try_into().ok()?);
    let data = member.get(20..)?;
    if data.len() != usize::try_from(size).ok()? {
        return None;
    }
    let symbol_end = data.iter().position(|&byte| byte == 0)?;
    let name = data.get(symbol_end + 1..)?;
    let name_end = name.iter().position(|&byte| byte == 0)?;
    std::str::from_utf8(&name[..name_end]).ok()
}

fn verify_archive(path: &Path) {
    let bytes =
        fs::read(path).unwrap_or_else(|error| panic!("Cannot read {}: {error}", path.display()));
    assert!(
        bytes.starts_with(b"!<arch>\n"),
        "Not a static archive: {}",
        path.display()
    );
    let mut position = 8;
    while position < bytes.len() {
        let header = &bytes[position..position + 60];
        let length: usize = std::str::from_utf8(&header[48..58])
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let member = &bytes[position + 60..position + 60 + length];
        let name = std::str::from_utf8(&header[..16]).unwrap().trim();
        // COFF short import objects: Sig1=0, Sig2=0xffff, Version=0.
        // Bigobj uses Version=2 and remains a valid static object.
        if !matches!(name, "/" | "//" | "/SYM64/") && member.starts_with(&[0, 0, 255, 255, 0, 0]) {
            let dll = coff_import_dll(member)
                .unwrap_or_else(|| panic!("Malformed COFF import object: {}", path.display()));
            // Rust 1.98's std bundles these raw-dylib imports even with
            // crt-static. Permit the exact system DLLs, not arbitrary import
            // libraries or dynamic CRTs. Packaging also audits the final EXE.
            assert!(
                ["bcryptprimitives.dll", "api-ms-win-core-synch-l1-2-0.dll"]
                    .iter()
                    .any(|system| dll.eq_ignore_ascii_case(system)),
                "DLL import library is not a static dependency: {} ({dll})",
                path.display()
            );
        }
        position += 60 + length + length % 2;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn import_object(dll: &str) -> Vec<u8> {
        let data = format!("symbol\0{dll}\0");
        let mut member = vec![0; 20];
        member[2..4].copy_from_slice(&u16::MAX.to_le_bytes());
        member[6..8].copy_from_slice(&0x8664_u16.to_le_bytes());
        member[12..16].copy_from_slice(&u32::try_from(data.len()).unwrap().to_le_bytes());
        member[18..20].copy_from_slice(&4_u16.to_le_bytes()); // IMPORT_OBJECT_NAME
        member.extend_from_slice(data.as_bytes());
        member
    }

    fn archive(members: &[(&str, &[u8])]) -> tempfile::NamedTempFile {
        let mut bytes = b"!<arch>\n".to_vec();
        for (name, member) in members {
            let header = format!(
                "{name:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
                0,
                0,
                0,
                "100644",
                member.len()
            );
            assert_eq!(header.len(), 60);
            bytes.extend_from_slice(header.as_bytes());
            bytes.extend_from_slice(member);
            if member.len() % 2 != 0 {
                bytes.push(b'\n');
            }
        }
        let file = tempfile::NamedTempFile::new().unwrap();
        fs::write(file.path(), bytes).unwrap();
        file
    }

    #[test]
    fn rust_static_archives_accept_only_the_known_system_dll_imports() {
        let random = import_object("BCRYPTPRIMITIVES.dll");
        let sync = import_object("api-ms-win-core-synch-l1-2-0.dll");
        let file = archive(&[
            ("/", &[0, 0, 255, 255, 0, 0]), // archive index, not an object
            ("code.obj/", &[0x64, 0x86, 0, 0]),
            ("big.obj/", &[0, 0, 255, 255, 2, 0]),
            ("/123", &random), // long archive name used by Rust's imports
            ("/456", &sync),
        ]);
        verify_archive(file.path());
    }

    #[test]
    fn rust_static_archives_still_reject_non_system_and_dynamic_crt_imports() {
        for dll in [
            "mradm_capi.dll",
            "libopenblas.dll",
            "VCRUNTIME140.dll",
            "api-ms-win-crt-runtime-l1-1-0.dll",
            "bcryptprimitives.dll.evil",
            r"C:\custom\bcryptprimitives.dll",
        ] {
            let allowed = import_object("bcryptprimitives.dll");
            let rejected = import_object(dll);
            let file = archive(&[("/123", &allowed), ("/456", &rejected)]);
            assert!(
                std::panic::catch_unwind(|| verify_archive(file.path())).is_err(),
                "accepted {dll}"
            );
        }
    }

    #[test]
    fn malformed_short_import_objects_are_rejected() {
        let valid = import_object("bcryptprimitives.dll");
        let mut unterminated = valid[..valid.len() - 1].to_vec();
        let size = u32::try_from(unterminated.len() - 20).unwrap();
        unterminated[12..16].copy_from_slice(&size.to_le_bytes());
        for member in [&valid[..6], &valid[..valid.len() - 1], &unterminated] {
            let file = archive(&[("/123", member)]);
            assert!(std::panic::catch_unwind(|| verify_archive(file.path())).is_err());
        }
    }

    #[test]
    fn every_library_file_of_the_manifest_is_audited_in_link_order() {
        let manifest = "file:/build/libmr_adm_audio.a\n\
            link:FLAC\n\
            target:FLAC=/build/_deps/libFLAC.a;ogg;/build/_deps/libogg.a;-lm\n\
            framework:CoreAudio\n\
            system:m\n";
        assert_eq!(
            libraries(manifest),
            [
                PathBuf::from("/build/libmr_adm_audio.a"),
                PathBuf::from("/build/_deps/libFLAC.a"),
                PathBuf::from("/build/_deps/libogg.a"),
            ]
        );
    }

    #[test]
    fn shared_native_libraries_are_rejected() {
        let file = tempfile::NamedTempFile::new().unwrap();
        fs::write(file.path(), "file:/opt/homebrew/lib/libFLAC.dylib\n").unwrap();
        let rejected = std::panic::catch_unwind(|| audit(file.path(), false));
        assert!(rejected.is_err());
        assert!(!is_archive(Path::new("C:/deps/FLAC.dll"), true));
        assert!(is_archive(Path::new("C:/deps/FLAC.lib"), true));
        assert!(!is_archive(Path::new("/deps/libFLAC.lib"), false));
    }

    #[test]
    #[should_panic(expected = "Unexpected native link manifest kind")]
    fn an_unknown_manifest_kind_is_rejected() {
        libraries("dylib:/usr/lib/libFLAC.so\n");
    }
}
