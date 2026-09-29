fn main() {
    println!("cargo:rerun-if-env-changed=FREECTIER_GITHUB_REPOSITORY");
    println!("cargo:rerun-if-env-changed=FREECTIER_UPDATER_PUBLIC_KEY");
    println!("cargo:rerun-if-changed=windows-app.manifest");
    // Generate a deterministic 32-bit ICO in OUT_DIR; no binary assets or
    // image tooling are needed to build a clean checkout.
    let path = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("freec.ico");
    let size = 32u32;
    let image_len = 40 + size * size * 4 + size * 4;
    let mut bytes = vec![0, 0, 1, 0, 1, 0, 32, 32, 0, 0, 1, 0, 32, 0];
    bytes.extend_from_slice(&image_len.to_le_bytes());
    bytes.extend_from_slice(&22u32.to_le_bytes());
    bytes.extend_from_slice(&40u32.to_le_bytes());
    bytes.extend_from_slice(&size.to_le_bytes());
    bytes.extend_from_slice(&(size * 2).to_le_bytes());
    bytes.extend_from_slice(&[1, 0, 32, 0]);
    bytes.extend_from_slice(&[0; 24]);
    for y in 0..size {
        for x in 0..size {
            let mark = (8..13).contains(&x) && (7..25).contains(&y)
                || (8..25).contains(&x) && (20..25).contains(&y)
                || (8..21).contains(&x) && (12..17).contains(&y);
            bytes.extend_from_slice(if mark {
                &[238, 238, 255, 255]
            } else {
                &[52, 40, 161, 255]
            });
        }
    }
    bytes.extend_from_slice(&[0; 128]);
    std::fs::write(&path, bytes).unwrap();
    tauri_build::try_build(
        tauri_build::Attributes::new().windows_attributes(
            tauri_build::WindowsAttributes::new()
                .window_icon_path(path)
                .app_manifest(include_str!("windows-app.manifest")),
        ),
    )
    .expect("Tauri build failed");
}
